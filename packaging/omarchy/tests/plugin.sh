#!/usr/bin/env bash

set -euo pipefail

plugin_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)
manifest="$plugin_dir/manifest.json"
widget="$plugin_dir/packaging/omarchy/BarWidget.qml"
service="$plugin_dir/packaging/omarchy/HyprcorrectService.qml"
qmltestrunner=/usr/lib/qt6/bin/qmltestrunner
qmllint=/usr/lib/qt6/bin/qmllint
[[ -x $qmltestrunner ]] || qmltestrunner=$(command -v qmltestrunner)
[[ -x $qmllint ]] || qmllint=$(command -v qmllint)

jq -e '.schemaVersion == 1
  and .id == "io.github.jondkinney.hyprcorrect"
  and .version == "1.0.1"
  and (.kinds == ["bar-widget"])
  and .entryPoints.barWidget == "packaging/omarchy/BarWidget.qml"
  and .barWidget.allowMultiple == false' "$manifest" >/dev/null

rg -q 'visible: hyprcorrect\.connected' "$widget"
rg -q 'textFormat: Text\.PlainText' "$widget"
rg -q 'stdout: SplitParser' "$service"
rg -q 'line\.length > 8192' "$plugin_dir/packaging/omarchy/CompanionSanitizer.js"
rg -q '\\u007f-\\u009f' "$plugin_dir/packaging/omarchy/CompanionSanitizer.js"
rg -q '\\u202a-\\u202e' "$plugin_dir/packaging/omarchy/CompanionSanitizer.js"
! rg -q 'StdioCollector|bash", "-c|sh", "-c' "$widget" "$service"
! rg -q 'os\.system|shell=True|subprocess' "$plugin_dir/packaging/omarchy/bin/hyprcorrect-companion"
rg -q 'O_NOFOLLOW.*O_NONBLOCK' "$plugin_dir/packaging/omarchy/bin/hyprcorrect-companion"

PYTHONDONTWRITEBYTECODE=1 python3 - "$plugin_dir/packaging/omarchy/bin/hyprcorrect-companion" <<'PY'
import ast
import pathlib
import sys

ast.parse(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
PY
PYTHONDONTWRITEBYTECODE=1 python3 "$plugin_dir/packaging/omarchy/tests/companion_security.py"
"$qmltestrunner" -input "$plugin_dir/packaging/omarchy/tests/tst_companion_sanitizer.qml"
"$qmllint" -I /usr/share/omarchy/shell "$widget" "$service"
