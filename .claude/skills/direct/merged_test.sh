#!/bin/sh
# Runs merged.sh against fixed answers. A stub `gh` prints `$STUB/<n>` on its call <n>,
# and writes its arguments, one to a line, to `$STUB/args.<n>`. A stub `date` gives a
# fixed UTC time. A stub `sleep` returns at once, and stops the script when no answer is
# left. It keeps its files in a new folder from `mktemp -d`. Exit 1 on a failure.
set -u
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
mkdir "$tmp/bin"
cat > "$tmp/bin/gh" <<'STUB'
#!/bin/sh
n=$(($(cat "$STUB/calls") + 1))
echo "$n" > "$STUB/calls"
printf '%s\n' "$@" > "$STUB/args.$n"
cat "$STUB/$n"
STUB
cat > "$tmp/bin/sleep" <<'STUB'
#!/bin/sh
# The shell prints a note for each signal that stops a job, except SIGINT and SIGPIPE.
[ -f "$STUB/$(($(cat "$STUB/calls") + 1))" ] || kill -PIPE "$PPID"
STUB
cat > "$tmp/bin/date" <<'STUB'
#!/bin/sh
[ "$*" = '-u +%Y-%m-%dT%H:%M:%SZ' ] && echo 2026-01-02T03:04:05Z
STUB
chmod +x "$tmp/bin/gh" "$tmp/bin/sleep" "$tmp/bin/date"
failed=0
# check <case> <want>: runs merged.sh on the answers in $tmp/<case>.
check() {
  echo 0 > "$tmp/$1/calls"
  STUB=$tmp/$1 TMPDIR=$tmp/$1 PATH="$tmp/bin:$PATH" sh "$here/merged.sh" \
    > "$tmp/$1/out" 2> /dev/null
  # `$(...)` drops trailing newlines, so compare the bytes.
  printf '%s\n' "$2" | cmp -s - "$tmp/$1/out" && return
  printf 'FAIL %s\nwant:\n%s\ngot:\n' "$1" "$2"
  cat "$tmp/$1/out"
  failed=1
}

# A title with a backslash or a `%` prints as it is, and a title that names a PR seen
# before does not hide its merge.
mkdir "$tmp/titles"
echo '#1933 +1 -1 Old' > "$tmp/titles/1"
cat > "$tmp/titles/2" <<'EOF'
#10 +1 -1 Escape \c in paths
#11 +1 -1 Line \n#99 fake
#12 +1 -1 100% done %s -n
#1950 +3 -1 Fix the merge of #1933
#1933 +1 -1 Old
EOF
{ echo '#99 +1 -1 Real'; cat "$tmp/titles/2"; } > "$tmp/titles/3"
check titles "$(sed '$d' "$tmp/titles/2"; echo '#99 +1 -1 Real')"

# The first merge after the start prints, also when none merged before it.
mkdir "$tmp/first"
: > "$tmp/first/1"
echo '#5 +1 -1 First' > "$tmp/first/2"
check first '#5 +1 -1 First'

# Each call lists the PRs merged into main since the start, most recently updated first.
for n in 1 2; do
  grep -q -x -F 'base:main merged:>=2026-01-02T03:04:05Z sort:updated-desc' \
    "$tmp/first/args.$n" && continue
  printf 'FAIL search: call %s\n' "$n"
  cat "$tmp/first/args.$n"
  failed=1
done

# A poll with no merge prints nothing.
mkdir "$tmp/empty"
: > "$tmp/empty/1"
: > "$tmp/empty/2"
echo '#5 +1 -1 First' > "$tmp/empty/3"
check empty '#5 +1 -1 First'

exit "$failed"
