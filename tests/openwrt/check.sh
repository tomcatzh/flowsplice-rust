#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"

sh -n "${repo_root}/openwrt/root/etc/init.d/flowsplice"
sh -n "${repo_root}/openwrt/root/usr/libexec/flowsplice/render-config"
sh -n "${repo_root}/openwrt/control/postinst"
sh -n "${repo_root}/openwrt/control/prerm"
python3 -m json.tool "${repo_root}/openwrt/root/usr/share/luci/menu.d/luci-app-flowsplice.json" >/dev/null
python3 -m json.tool "${repo_root}/openwrt/root/usr/share/rpcd/acl.d/luci-app-flowsplice.json" >/dev/null
node -e 'new Function(require("fs").readFileSync(process.argv[1], "utf8"))' \
  "${repo_root}/openwrt/root/www/luci-static/resources/view/flowsplice.js"
python3 "${repo_root}/tests/openwrt/test_ipk.py"
python3 "${repo_root}/tests/openwrt/test_i18n.py"
python3 "${repo_root}/tests/openwrt/test_renderer.py"
