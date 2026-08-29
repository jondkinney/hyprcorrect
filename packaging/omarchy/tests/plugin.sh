#!/usr/bin/env bash

set -euo pipefail

plugin_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)
manifest="$plugin_dir/manifest.json"
widget="$plugin_dir/packaging/omarchy/BarWidget.qml"
service="$plugin_dir/packaging/omarchy/HyprcorrectService.qml"

jq -e '.schemaVersion == 1
  and .id == "io.github.jondkinney.hyprcorrect"
  and (.kinds == ["bar-widget"])
  and .entryPoints.barWidget == "packaging/omarchy/BarWidget.qml"
  and .barWidget.allowMultiple == false' "$manifest" >/dev/null

rg -q 'visible: hyprcorrect\.connected' "$widget"
rg -q 'textFormat: Text\.PlainText' "$widget"
rg -q 'stdout: SplitParser' "$service"
rg -q 'line\.length > 8192' "$service"
rg -q '\\u007f-\\u009f' "$service"
rg -q '\\u202a-\\u202e' "$service"
! rg -q 'StdioCollector|bash", "-c|sh", "-c' "$widget" "$service"

bash -n "$plugin_dir/packaging/omarchy/bin/hyprcorrect-companion"
qmllint -I /usr/share/omarchy/shell "$widget" "$service"
