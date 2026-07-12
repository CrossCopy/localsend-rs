#!/bin/sh
set -eu
mkdir -p /shared/received
# --no-https keeps this scenario on plain HTTP (the healthcheck curls http://).
# A dedicated HTTPS e2e scenario can drop this flag.
exec localsend-rs receive --directory /shared/received --port 53317 --auto-accept --no-https
