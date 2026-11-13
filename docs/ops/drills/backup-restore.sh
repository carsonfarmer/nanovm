#!/bin/bash
# Backup and restore drill (runbooks.md): write, stop, scripts/backup.py backup,
# restore into a fresh root, start on it, read and write. Needs restic and a
# dev build; run as root to see app uids preserved:
#   docs/ops/drills/backup-restore.sh target/debug .
set -u
BIN=$1 REPO=${2:-.}; PORT=28680; REG=25680
W=$(mktemp -d /var/tmp/nanovm-backup.XXXXXX); chmod 755 $W
pids=()
trap 'kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null; rm -rf $W' EXIT
$BIN/examples/dev-registry 127.0.0.1:$REG > $W/apps 2>/dev/null & pids+=($!)
sleep 2
for a in counter hostile; do printf 'nanovm-app: %s\nnanovm-trust: dev dev\n' "$(awk -v a=$a '$1==a{print $2}' $W/apps)" > $W/$a.h; done
start() { printf 'listen = "127.0.0.1:%s"\ndata_dir = "%s"\ndev = true\ntelemetry_endpoint = "http://127.0.0.1:9"\n' $PORT $1 > $W/n.toml
  env -u RUST_BACKTRACE $BIN/nanovm serve --config $W/n.toml > $W/n.log 2>&1 & NODE=$!; sleep 1; }
U=http://127.0.0.1:$PORT
start $W/srv
for _ in 1 2 3; do curl -s -o /dev/null -XPOST -H @$W/counter.h $U/a/incr; done
curl -s -o /dev/null -H @$W/hostile.h "$U/h/commit?v=kept"
echo "before: $(curl -s -H @$W/counter.h $U/a/) $(curl -s -H @$W/hostile.h $U/h/commit)"
kill $NODE; wait $NODE 2>/dev/null; sleep 0.5
export RESTIC_REPOSITORY=$W/restic RESTIC_PASSWORD_FILE=$W/pw
echo secret > $W/pw; restic init -q
id=$(python3 $REPO/scripts/backup.py backup $W/srv --stopped) && echo "snapshot ${id:0:12}…"
mv $W/srv $W/srv.old
python3 $REPO/scripts/backup.py restore "$id" $W/srv && echo "restored"
ls -ln $W/srv/objects | tail -2
start $W/srv
echo "after: $(curl -s -H @$W/counter.h $U/a/) $(curl -s -H @$W/hostile.h $U/h/commit)"
echo "write: $(curl -s -XPOST -H @$W/counter.h $U/a/incr)"
kill $NODE
