#!/usr/bin/env bash
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

blocks="${1:-1}"
wait_for_zcashd
wait_for_lightwalletd
zcash_cli generate "$blocks"
wait_for_lightwalletd_tip "$(zcash_cli getblockcount)"
