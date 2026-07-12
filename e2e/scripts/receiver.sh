#!/bin/sh
set -eu
mkdir -p /shared/received
exec localsend-rs receive --directory /shared/received --port 53317 --auto-accept
