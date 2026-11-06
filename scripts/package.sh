#!/usr/bin/env bash
# 构建目标平台二进制并交给官方 CLI 生成安装包
set -euo pipefail

task_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
task_cli=${CPR_PLUGIN_CLI:-cpr-plugin}
task_target=${CPR_PLUGIN_TARGET:-x86_64-unknown-linux-gnu}

cargo build --manifest-path "$task_root/Cargo.toml" --release --locked --target "$task_target"
python3 "$task_root/scripts/licenses.py" --target "$task_target"
"$task_cli" package \
  --manifest "$task_root/plugin.json" \
  --binary "$task_root/target/$task_target/release/cpr-session-failover" \
  --target "$task_target" \
  --output-dir "$task_root/dist"
