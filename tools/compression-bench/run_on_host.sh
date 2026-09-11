#!/bin/sh
# Run only the uploaded standalone benchmark; never installs/restarts services.
set -u
umask 077
cd "${1:?provide the dedicated temporary benchmark directory}" || exit 2
mkdir -p result || exit 2

services() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl list-units --type=service --state=running --no-legend --plain |
            awk '{print $1}' |
            xargs -r systemctl show -p Id -p MainPID -p ActiveState -p NRestarts
    else
        for entry in /proc/[0-9]*/comm; do
            pid=${entry#/proc/}; pid=${pid%/comm}
            name=$(cat "$entry" 2>/dev/null) || continue
            case "$name" in
                flowsplice-*|dnsmasq|odhcpd|uhttpd|rpcd|netifd|mihomo|procd|dropbear)
                    printf '%s %s\n' "$pid" "$name" ;;
            esac
        done
    fi
}

snapshot() {
    suffix=$1
    {
        date -u '+%Y-%m-%dT%H:%M:%SZ'
        uname -srm
        cat /etc/os-release
        cat /proc/loadavg
        cat /proc/meminfo
        awk '/^(model name|cpu MHz|CPU implementer|CPU part|CPU revision|Features|flags|processor)[[:space:]]*:/' /proc/cpuinfo
        if [ -r /proc/device-tree/model ]; then
            tr '\000' '\n' </proc/device-tree/model
        fi
        for entry in /sys/devices/system/cpu/cpufreq/policy*/cpuinfo_max_freq \
            /sys/devices/system/cpu/cpufreq/policy*/scaling_governor; do
            [ -r "$entry" ] && { printf '%s=' "$entry"; cat "$entry"; }
        done
        df -Pk .
    } >"result/environment-$suffix.txt"
    cat /proc/stat >"result/proc-stat-$suffix.txt"
    services >"result/services-$suffix.txt" 2>/dev/null
}

snapshot before
sha256sum benchmark frozen-corpus.bin >result/payload-sha256.txt
(
    # A 256 MiB virtual-memory ceiling protects small hosts from unexpected growth.
    ulimit -v 262144 || exit 88
    # nice lets existing interactive/system work take precedence. Wall and CPU time
    # are measured separately, so descheduling remains visible in the report.
    timeout 900 nice -n 10 ./benchmark --bundle frozen-corpus.bin result 5 120
) >result/run.log 2>&1
status=$?
printf '%s\n' "$status" >result/exit-code.txt
snapshot after
tar -czf result.tar.gz result || exit 3
printf 'benchmark_exit_code=%s\n' "$status"
exit "$status"
