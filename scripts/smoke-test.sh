#!/usr/bin/env bash
# Cross-platform end-to-end smoke test for sqlitefs.
# Usage: smoke-test.sh <mount_point> <binary_path>
# Exits non-zero on any failure.
set -euo pipefail

MNT="${1:?Usage: smoke-test.sh <mount_point> <binary_path>}"
BINARY="${2:?Usage: smoke-test.sh <mount_point> <binary_path>}"
FAILED=0
OS="$(uname)"

# --- OS-specific helpers ---

is_mounted() {
    if [[ "$OS" == "Darwin" ]]; then
        mount | grep -qF " on $1 "
    else
        mountpoint -q "$1"
    fi
}

stat_size()   { [[ "$OS" == "Darwin" ]] && stat -f %z  "$1" || stat -c %s "$1"; }
stat_nlink()  { [[ "$OS" == "Darwin" ]] && stat -f %l  "$1" || stat -c %h "$1"; }
stat_blocks() { [[ "$OS" == "Darwin" ]] && stat -f %b  "$1" || stat -c %b "$1"; }

unmount_fs() {
    if [[ "$OS" == "Darwin" ]]; then
        umount "$1" 2>/dev/null || true
    else
        fusermount3 -u "$1" 2>/dev/null || true
    fi
}

# setfattr/getfattr are Linux attr-package tools; not available on macOS by default.
HAVE_XATTR=false
command -v setfattr &>/dev/null && command -v getfattr &>/dev/null && HAVE_XATTR=true

mkdir -p "$MNT"

# --- 1. Start sqlite-fs and wait for mount ---
"$BINARY" "$MNT" &
FS_PID=$!
trap 'unmount_fs "$MNT"; kill $FS_PID 2>/dev/null || true' EXIT

for _ in $(seq 1 50); do
    is_mounted "$MNT" && break
    sleep 0.1
done
is_mounted "$MNT" || { echo "FAIL: mount never became live"; exit 1; }

check() {  # check "label" "expected" "actual"
    if [[ "$2" == "$3" ]]; then
        echo "  ok   $1"
    else
        echo "  FAIL $1: expected=$2 actual=$3"
        FAILED=$((FAILED+1))
    fi
}

# --- 2. Feature coverage: exercise each handler type ---
echo "== feature coverage =="

# Basic file I/O (open/write/read/release/unlink)
echo hello > "$MNT/greeting"
check "file round-trip" "hello" "$(cat "$MNT/greeting")"
rm "$MNT/greeting"
check "unlink" "0" "$(ls "$MNT" | wc -l | tr -d ' ')"

# Directories (mkdir/readdir/rmdir)
mkdir "$MNT/d1"
touch "$MNT/d1/a" "$MNT/d1/b"
check "readdir count" "2" "$(ls "$MNT/d1" | wc -l | tr -d ' ')"
rm "$MNT/d1/a" "$MNT/d1/b"
rmdir "$MNT/d1"

# Hard link (link/nlink bookkeeping)
echo one > "$MNT/orig"
# stat forces the kernel to replace the O_CREAT negative dentry with a positive one;
# without this, linkat may find the stale negative dentry and return ENOENT on Linux.
stat "$MNT/orig" > /dev/null
ln "$MNT/orig" "$MNT/hardlink"
check "hardlink nlink" "2" "$(stat_nlink "$MNT/orig")"
rm "$MNT/hardlink"
check "nlink after unlink" "1" "$(stat_nlink "$MNT/orig")"
rm "$MNT/orig"

# Symlink + readlink
ln -s target-does-not-exist "$MNT/sym"
check "readlink" "target-does-not-exist" "$(readlink "$MNT/sym")"
rm "$MNT/sym"

# Rename (move_dentry)
echo renameme > "$MNT/before"
mv "$MNT/before" "$MNT/after"
check "rename preserves content" "renameme" "$(cat "$MNT/after")"
rm "$MNT/after"

# xattr (set/get/list/remove) — requires attr package (Linux); skipped elsewhere
if $HAVE_XATTR; then
    touch "$MNT/xf"
    setfattr -n user.foo -v bar "$MNT/xf"
    check "getxattr" "bar" "$(getfattr -n user.foo --only-values "$MNT/xf" 2>/dev/null)"
    setfattr -x user.foo "$MNT/xf"
    rm "$MNT/xf"
else
    echo "  skip xattr (setfattr/getfattr not available on this OS)"
fi

# --- 3. Regression-anchored: replay V0x scenarios ---
echo "== regression anchors =="

# V01: u32 size field would truncate 5 GiB to ~1 GiB (5368709120 mod 2^32 = 1073741824)
truncate -s 5G "$MNT/huge"
check "V01 5 GiB file size" "5368709120" "$(stat_size "$MNT/huge")"
rm "$MNT/huge"

# V02: u32 offset would wrap at ~4.29 GiB. seek=4100 MiB = 4299161600 B, past u32::MAX.
dd if=/dev/zero of="$MNT/sparse" bs=1M seek=4100 count=1 status=none
check "V02 sparse write size" "4300210176" "$(stat_size "$MNT/sparse")"  # (4100+1) * 1 MiB
rm "$MNT/sparse"

# V03: zero-length write used to underflow (offset + size - 1 wraps). Must not hang or error.
touch "$MNT/z"
: > "$MNT/z"
dd if=/dev/null of="$MNT/z" status=none
check "V03 zero-length write" "0" "$(stat_size "$MNT/z")"
rm "$MNT/z"

# V06: symlink target > 4096 bytes must be rejected cleanly, leaving no orphan dentry
LONG_TARGET=$(printf 'a%.0s' $(seq 1 5000))
if ln -s "$LONG_TARGET" "$MNT/bad-sym" 2>/dev/null; then
    echo "  FAIL V06: oversized symlink target was accepted"
    FAILED=$((FAILED+1))
    rm "$MNT/bad-sym"
else
    echo "  ok   V06 oversized symlink rejected"
fi
check "V06 no orphan dentry" "0" "$(ls "$MNT" 2>/dev/null | grep -c bad-sym || true)"

# V08: lookup block count — write 3 blocks, verify per-file count (GROUP BY fix).
# sqlitefs reports blocks in native 4096-byte units without converting to the POSIX
# 512-byte st_blocks unit, so stat returns 3 (not 24). The important invariant is
# that this is THIS file's count only, not a cumulative total (the GROUP BY bug).
dd if=/dev/zero of="$MNT/blockcheck" bs=4096 count=3 status=none
check "V08 block count" "3" "$(stat_blocks "$MNT/blockcheck")"
rm "$MNT/blockcheck"

# V11: removexattr on a missing key must fail with ENODATA, not succeed silently
if $HAVE_XATTR; then
    touch "$MNT/v11"
    if setfattr -x user.nonexistent "$MNT/v11" 2>/dev/null; then
        echo "  FAIL V11: removexattr on missing key returned success"
        FAILED=$((FAILED+1))
    else
        echo "  ok   V11 removexattr missing key rejected"
    fi
    rm "$MNT/v11"
else
    echo "  skip V11 (setfattr not available on this OS)"
fi

# V12: mv -n (no-clobber) must not overwrite an existing destination
echo original > "$MNT/dest"
echo newdata  > "$MNT/src"
mv -n "$MNT/src" "$MNT/dest" 2>/dev/null || true
check "V12 noreplace preserves dest" "original" "$(cat "$MNT/dest")"
rm -f "$MNT/src" "$MNT/dest"

# --- 4. Report ---
echo
if [[ $FAILED -eq 0 ]]; then
    echo "ALL GREEN"
    exit 0
else
    echo "$FAILED check(s) failed"
    exit 1
fi
