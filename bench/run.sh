#!/usr/bin/env bash
# Scans a generated 1M-file tree and fails when apdu's peak memory or quit
# time goes over bench/baseline plus a margin. `--update` saves what it
# measured as the new baseline. Needs tmux, to quit the TUI the way a person does.
set -euo pipefail
cd "$(dirname "$0")/.."

fixture=${APDU_BENCH_DIR:-/tmp/apdu-bench}
bin=$PWD/target/release/apdu
session=apdu-bench-$$
rss_margin_pct=30
quit_margin_ms=500

now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'; }

wait_for() {
  local deadline=$((SECONDS + $2))
  until eval "$1"; do
    ((SECONDS < deadline)) || { echo "timed out waiting for: $1" >&2; exit 1; }
    sleep 0.05
  done
}

cargo build --release --locked --quiet

# 100 projects of 10k files each; 80% sit in node_modules and .git, as on a
# developer's disk, so the scan has something to collapse.
if [[ ! -f $fixture.done ]]; then
  rm -rf "$fixture"
  perl -e '
    my $root = shift;
    for my $p (0 .. 99) {
      for ([src => 20], [node_modules => 60], [".git/objects" => 20]) {
        my ($dir, $n) = @$_;
        for my $d (0 .. $n - 1) {
          my $path = "$root/project-$p/$dir/group-$d";
          system("mkdir", "-p", $path) == 0 or die;
          for my $f (0 .. 99) {
            open my $fh, ">", "$path/component-file-$f.js" or die "$path: $!";
            print $fh "x";
          }
        }
      }
    }' "$fixture"
  touch "$fixture.done"
fi

out=$(mktemp)
/usr/bin/time -l "$bin" --no-cache --print 1 "$fixture" >/dev/null 2>"$out"
rss_mb=$(awk '/maximum resident set size/ { printf "%d", $1 / 1048576 }' "$out")
rm -f "$out"

exited=$(mktemp)
rm -f "$exited"
tmux new-session -d -s "$session" -x 120 -y 30 "'$bin' --no-cache '$fixture'; perl -MTime::HiRes=time -e 'printf \"%d\", time * 1000' > '$exited'"
trap 'tmux kill-session -t "$session:" 2>/dev/null || true; rm -f "$exited"' EXIT
wait_for "tmux capture-pane -p -t '$session:' | grep -q 'scanned in'" 600
sleep 1
start=$(now_ms)
tmux send-keys -t "$session:" q
wait_for "[[ -s '$exited' ]]" 60
quit_ms=$(($(cat "$exited") - start))

echo "peak_rss_mb $rss_mb"
echo "quit_ms $quit_ms"
if [[ ${1:-} == --update ]]; then
  printf 'peak_rss_mb %s\nquit_ms %s\n' "$rss_mb" "$quit_ms" >bench/baseline
  echo "saved to bench/baseline"
  exit 0
fi

base_rss=$(awk '$1 == "peak_rss_mb" { print $2 }' bench/baseline)
base_quit=$(awk '$1 == "quit_ms" { print $2 }' bench/baseline)
max_rss=$((base_rss * (100 + rss_margin_pct) / 100))
max_quit=$((base_quit + quit_margin_ms))
fail=0
((rss_mb <= max_rss)) || { echo "peak memory ${rss_mb} MB is over ${max_rss} MB (baseline ${base_rss} + ${rss_margin_pct}%)"; fail=1; }
((quit_ms <= max_quit)) || { echo "quit took ${quit_ms} ms, over ${max_quit} ms (baseline ${base_quit} + ${quit_margin_ms} ms)"; fail=1; }
((fail == 0)) && echo "within the baseline (memory up to ${max_rss} MB, quit up to ${max_quit} ms)"
exit "$fail"
