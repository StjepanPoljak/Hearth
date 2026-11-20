#!/bin/sh
# Proactive reclaim: every INTERVAL seconds, asks the kernel to reclaim
# STEP from the whole guest (the root cgroup's memory.reclaim) while the
# guest shows no sign of needing that memory: memory pressure (PSI
# "some", stalled time) and file refaults since the last round both
# below their limits. Otherwise it skips BACKOFF rounds. Reclaimed page
# cache becomes free memory, which free page reporting gives the host.
# Needs CONFIG_PSI and cgroup v2. Not in the rootfs yet.
INTERVAL=${INTERVAL:-10}
STEP=${STEP:-32M}
PSI_MAX_US=${PSI_MAX_US:-10000}         # 0.1% of a 10 s round
REFAULT_MAX=${REFAULT_MAX:-256}         # pages per round, 1M
BACKOFF=${BACKOFF:-6}

psi() { sed -n 's/^some .*total=//p' /proc/pressure/memory; }
refaults() { sed -n 's/^workingset_refault_file //p' /proc/vmstat; }

p0=$(psi); r0=$(refaults); skip=0
while sleep "$INTERVAL"; do
    p1=$(psi); r1=$(refaults)
    dp=$((p1 - p0)); dr=$((r1 - r0)); p0=$p1; r0=$r1
    if [ $dp -gt $PSI_MAX_US ] || [ $dr -gt $REFAULT_MAX ]; then
        skip=$BACKOFF
        echo "$(date +%s) back off: psi ${dp}us refaults $dr"
    elif [ $skip -gt 0 ]; then
        skip=$((skip - 1))
    else
        echo "$STEP" > /sys/fs/cgroup/memory.reclaim 2>/dev/null
        echo "$(date +%s) reclaim $STEP: $?"
    fi
done
