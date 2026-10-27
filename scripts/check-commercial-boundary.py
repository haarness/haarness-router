#!/usr/bin/env python3
"""Fail when a file, a path or a commit message carries a word this product must not.

The words are kept as SHA-256 hashes, so the list itself says nothing. A file is
split into words the way a reader sees them: camelCase, snake_case, kebab-case
and digits all separate words, and case is ignored. Two or three camelCase parts
are also read joined, so MyBrand is the word "mybrand". Each word is hashed and
looked up; so is a lone "x" joined to the next word by "-" or "_" (a header
prefix such as X-Name), but not "Mac OS X) Name" in a user agent.

Usage:
  check-commercial-boundary.sh                 check tracked and untracked files
  check-commercial-boundary.sh --commits A..B  also check those commit messages
  check-commercial-boundary.sh --self-test     check the checker

To add a word, append the hex SHA-256 of its lowercase form to BLOCKED.
"""

import hashlib
import os
import re
import subprocess
import sys
import tempfile

BLOCKED = frozenset(
    {
        "e9fd5bad270cc660bb332cec16c48a84cc151d9a7d55d9c8c824e1423fcc38bf",
        "1bf5abcffb605154a20600c92020ef11cd5e6d84c854d20991c5c35e56e985c3",
        "d1377163a5b898f138a44be7aa6ad8496f0beff6f808e1f6d40e8cadbbc78dd1",
        "1733c153cef689df0f24d6bec34ae4bbe36f8a8d2787d756f1d3d5e38b7ab8d7",
        "4d1d9d2445e197e6a450bb6cae9688fb86ae561aef50c263544a1869dda7ea71",
        "42aa616cf1fc7feb39d221ba3271601be9181baec22c1e3d09c1fe0ae1e35878",
        "925e2b1a37e8212265f53faaeff8f154ddc9d1e20e0cc021ec206d0a65cc5e4e",
        "3e9635f85b49d99b83907c185d24d167405d975cf5c97964467f7ef74af71d0b",
        "9c3c5390b624654379e191163805b6d2f71f429996cb1e6754c2c06db08f6669",
        "358071666ca25a6a1f763c7bd9d93d7fa727efab58306e62c8265afa55f3a890",
        "d35b5a7b3cc88885bc4ec8970bd4c6c6f1263b69f51ffd94a9092a97bfbfebd3",
        "f7374749085b3d6d7cac06ac15749808e3a31eb6783b855c909be40b33ce166b",
    }
)
# A word that exists only so the self-test can prove the machinery works.
CANARY = "af8a205b0bde778a19fcfcdf47ff617ceceb772bbe77394ec8e2f0a42df96a77"

LOWER_UPPER = re.compile(r"([a-z0-9])([A-Z])")
UPPER_RUN = re.compile(r"([A-Z]+)([A-Z][a-z])")
# Hashes, checksums and base64 blobs: random letters, not words.
OPAQUE = re.compile(r"[A-Za-z0-9+/]{40,}={0,2}")


def _opaque(match: "re.Match") -> str:
    run = match.group(0)
    mixed = re.search(r"[a-z]", run) and re.search(r"[A-Z]", run) and re.search(r"[0-9]", run)
    return " " if mixed else run
RUN = re.compile(r"[A-Za-z0-9]+")
TOKEN = re.compile(r"[a-z0-9]+")
LETTERS = re.compile(r"[a-z]+")
# Split camelCase with "_" so XName reads as a header prefix, like X-Name and X_Name.
HEADER = re.compile(r"(?<![a-z0-9])x[-_]([a-z0-9]+)")

# Digests, not verdicts: callers check against different lists.
_digests: dict = {}
_candidates: dict = {}


def _camel(text: str) -> str:
    return UPPER_RUN.sub(r"\1_\2", LOWER_UPPER.sub(r"\1_\2", text)).lower()


def _blocked(word: str, blocked: frozenset) -> bool:
    digest = _digests.get(word)
    if digest is None:
        digest = hashlib.sha256(word.encode()).hexdigest()
        _digests[word] = digest
    return digest in blocked


def _words(run: str) -> tuple:
    words = _candidates.get(run)
    if words is None:
        parts = TOKEN.findall(_camel(run))
        # A word written in camelCase is still one word: MyBrand, useMyBrandAuth.
        joined = ["".join(parts[i:j]) for i in range(len(parts)) for j in range(i + 1, min(i + 3, len(parts)) + 1)]
        # Digits end a word too ("name2").
        words = tuple({w for word in joined for w in [word, *([] if word.isalpha() else LETTERS.findall(word))]})
        _candidates[run] = words
    return words


def flagged(text: str, blocked: frozenset = BLOCKED) -> bool:
    text = OPAQUE.sub(_opaque, text)
    if any(_blocked(word, blocked) for run in RUN.findall(text) for word in _words(run)):
        return True
    return any(_blocked("x " + match.group(1), blocked) for match in HEADER.finditer(_camel(text)))


def git(repository: str, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", repository, *args], check=True, capture_output=True, text=True
    ).stdout


def check_files(repository: str, blocked: frozenset = BLOCKED) -> list:
    # Untracked files count too: the gate must catch a new file before it is staged.
    names = git(repository, "ls-files", "-z", "--cached", "--others", "--exclude-standard")
    problems = []
    for name in sorted(set(filter(None, names.split("\0")))):
        if name.startswith("target/") or "/target/" in name:
            continue
        if flagged(name, blocked):
            problems.append(f"{name}: the path")
        path = os.path.join(repository, name)
        if not os.path.isfile(path) or os.path.islink(path):
            continue
        with open(path, "rb") as handle:
            data = handle.read()
        if b"\0" in data[:8192]:
            continue
        for number, line in enumerate(data.decode("utf-8", "replace").splitlines(), 1):
            if flagged(line, blocked):
                problems.append(f"{name}:{number}: {line.strip()[:160]}")
    return problems


def check_commits(repository: str, commits: str, blocked: frozenset = BLOCKED) -> list:
    log = git(repository, "log", "--format=%h%x00%B%x01", commits)
    problems = []
    for entry in filter(str.strip, log.split("\x01")):
        sha, _, message = entry.strip("\n").partition("\0")
        if flagged(message, blocked):
            problems.append(f"commit {sha}: {message.strip().splitlines()[0][:160]}")
    return problems


def self_test() -> int:
    blocked = frozenset({CANARY})
    with tempfile.TemporaryDirectory(prefix="haarness-commercial-boundary.") as root:
        subprocess.run(["git", "init", "-q", root], check=True)
        source = os.path.join(root, "src")
        os.makedirs(source)
        lib = os.path.join(source, "lib.rs")

        def write(text: str) -> None:
            with open(lib, "w") as handle:
                handle.write(text + "\n")

        write(
            'const TARGET: &str = "aarch64-apple-darwin";\n'
            'const FONT: &str = "-apple-system";\n'
            'const PLIST_DTD: &str = "-//Apple//DTD PLIST 1.0//EN";\n'
            'const RG: &str = "rg-15.2.0-x86_64-apple-darwin.gz";\n'
            '"integrity": "sha512-EWazOblFYUvlGZcfGhPUPmYh3nikUxBVb+y9MJun5f3hBi812X+8MSQTujLBtgK3cf51fJWbWfOjyeO954d+Eg=="\n'
            'const UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15";'
        )
        if check_files(root) or check_files(root, blocked):
            print("Commercial boundary self-test failed: allowed platform strings were rejected.", file=sys.stderr)
            return 1
        header = frozenset({hashlib.sha256(b"x zzboundarypair").hexdigest()})
        for form, expected in (("X-Zzboundarypair-Token: 1", True), ("x_zzboundarypair", True),
                               ("XZzboundarypairHeader", True), ("like Mac OS X) Zzboundarypair/605", False),
                               ("max-zzboundarypair zzboundarypair", False)):
            write(form)
            if bool(check_files(root, header)) != expected:
                verdict = "was not detected" if expected else "was rejected"
                print(f"Commercial boundary self-test failed: {form} {verdict}.", file=sys.stderr)
                return 1
        for form in ("zzboundarycanary", "Zzboundarycanary", "readZzboundarycanaryConfig",
                     "ZZBOUNDARYCANARY_HOME", "zzboundarycanary-state", "zzboundarycanary2",
                     "ZzboundaryCanary", "useZzboundaryCanaryAuth",
                     "/Users/someone/work/projectfolder/zzboundarycanary/state"):
            write(form)
            if not check_files(root, blocked):
                print(f"Commercial boundary self-test failed: {form} was not detected.", file=sys.stderr)
                return 1
        write("clean")
        with open(os.path.join(root, "zzboundarycanary-notes.txt"), "w") as handle:
            handle.write("clean\n")
        if not check_files(root, blocked):
            print("Commercial boundary self-test failed: a flagged path was missed.", file=sys.stderr)
            return 1
        os.remove(os.path.join(root, "zzboundarycanary-notes.txt"))
        subprocess.run(["git", "-C", root, "add", "."], check=True)
        env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t", GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t")
        subprocess.run(["git", "-C", root, "commit", "-q", "-m", "Port the zzboundarycanary fix"], check=True, env=env)
        if not check_commits(root, "HEAD", blocked):
            print("Commercial boundary self-test failed: a commit message was missed.", file=sys.stderr)
            return 1
        if len(BLOCKED) == 0 or any(not re.fullmatch(r"[0-9a-f]{64}", h) for h in BLOCKED):
            print("Commercial boundary self-test failed: the blocked list is malformed.", file=sys.stderr)
            return 1
    print("Commercial boundary self-test passed.")
    return 0


def main(argv: list) -> int:
    if argv == ["--self-test"]:
        return self_test()
    commits = None
    if len(argv) == 2 and argv[0] == "--commits":
        commits = argv[1]
    elif argv:
        print("usage: check-commercial-boundary.sh [--self-test | --commits <range>]", file=sys.stderr)
        return 2
    repository = git(os.getcwd(), "rev-parse", "--show-toplevel").strip()
    problems = check_files(repository)
    if commits:
        problems += check_commits(repository, commits)
    if problems:
        print("Commercial boundary check failed: blocked words were found.", file=sys.stderr)
        print("\n".join(problems), file=sys.stderr)
        return 1
    print("Commercial boundary check passed.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
