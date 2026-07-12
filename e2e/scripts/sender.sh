#!/bin/sh
set -eu

SIZE_KB="${PAYLOAD_KB:-1024}"
mkdir -p /work
dd if=/dev/urandom of=/work/payload.bin bs=1024 count="$SIZE_KB" 2>/dev/null
WANT=$(sha256sum /work/payload.bin | cut -d' ' -f1)

echo "waiting for receiver..."
i=0
until curl -fsS "http://receiver:53317/api/localsend/v2/info" >/dev/null 2>&1; do
  i=$((i + 1))
  [ "$i" -ge 60 ] && { echo "receiver never became ready"; exit 1; }
  sleep 1
done

RECEIVER_IP=$(getent hosts receiver | awk '{print $1}' | head -n1)
echo "sending to $RECEIVER_IP"
localsend-rs send "$RECEIVER_IP" /work/payload.bin

i=0
until [ -f /shared/received/payload.bin ]; do
  i=$((i + 1))
  [ "$i" -ge 30 ] && { echo "file never arrived"; ls -la /shared/received; exit 1; }
  sleep 1
done
GOT=$(sha256sum /shared/received/payload.bin | cut -d' ' -f1)

if [ "$WANT" = "$GOT" ]; then
  echo "E2E-PASS send-direct ($SIZE_KB KB, sha256 match)"
else
  echo "E2E-FAIL sha mismatch: want=$WANT got=$GOT"
  exit 1
fi
