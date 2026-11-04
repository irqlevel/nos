#!/bin/bash
# The benchmark suite of the KVM-vs-nos comparison, the same disk image under
# both hypervisors. bench.service runs it once at boot when /opt/bench/AUTORUN
# exists (QUICK beside it: every step cut short, to check the suite itself).
# Everything goes to /var/lib/bench/run; the numbers as "BENCH ..." lines on
# the serial console as they come; at the end the whole directory as a
# base64 tar.xz between BENCH-ARCHIVE-BEGIN and -END, then the guest powers
# itself off -- which is how either host knows it is done.
set -u
export LC_ALL=C
B=/opt/bench
OUT=/var/lib/bench/run
CON=/dev/ttyS0
QUICK=0
[ -e $B/QUICK ] && QUICK=1
# ONLY beside it: the steps it names alone, a word each (sysbench-cpu 7zip
# openssl sysbench-memory perf-bench cyclictest fio kbuild pgbench) -- a run
# of a few minutes for the step a change is about, the rest left out
ONLY=
[ -e $B/ONLY ] && ONLY=" $(tr -s ' \n' '  ' < $B/ONLY) "
rm -f $B/AUTORUN
rm -rf $OUT
mkdir -p $OUT
exec >> $OUT/run.log 2>&1

con() { printf '%s\n' "$*" > $CON; }
say() { printf 'BENCH %s\n' "$*" | tee -a $OUT/summary.txt > $CON; }
step() { con "BENCH-STEP $(date +%T) $*"; echo "== $(date +%T) $*"; }
# want <step>: whether this run takes the step -- every one, but for ONLY
want() { [ -z "$ONLY" ] || case "$ONLY" in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
# d <full> <quick>: a length or a count, cut short in a quick run
d() { if [ $QUICK = 1 ]; then echo "$2"; else echo "$1"; fi; }
now_ms() { echo $(( $(date +%s%N) / 1000000 )); }

step "start quick=$QUICK only=[${ONLY# }]"
systemctl is-system-running --wait > $OUT/system-state.txt 2>&1
systemctl --failed --no-legend > $OUT/failed-units.txt 2>&1
sleep "$(d 60 3)"
sync
echo 3 > /proc/sys/vm/drop_caches

step info
say kernel "$(uname -r)"
say cpu "$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ *//')"
say nproc "$(nproc)" mem_kib "$(awk '/MemTotal/ {print $2}' /proc/meminfo)"
say clocksource "$(cat /sys/devices/system/clocksource/clocksource0/current_clocksource)"
say cpuidle "$(cat /sys/devices/system/cpu/cpuidle/current_driver 2>/dev/null || echo none)"
say hypervisor "$(dmesg | grep -m1 'Hypervisor detected' | sed 's/^\[[^]]*\] //')"
say cmdline "$(cat /proc/cmdline)"
say boot "$(systemd-analyze 2>&1 | head -1)"
say state "$(cat $OUT/system-state.txt) failed=[$(awk '{print $2}' $OUT/failed-units.txt | xargs)]"
say vda "queues=$(ls /sys/block/vda/mq | wc -l) nr_requests=$(cat /sys/block/vda/queue/nr_requests) max_sectors_kb=$(cat /sys/block/vda/queue/max_sectors_kb) wc=$(cat /sys/block/vda/queue/write_cache) discard_max=$(cat /sys/block/vda/queue/discard_max_bytes)"
say thp "$(cat /sys/kernel/mm/transparent_hugepage/enabled)"
say vuln "$(grep -H . /sys/devices/system/cpu/vulnerabilities/* | sed 's|.*/||' | tr '\n' ';')"
say flags "$(grep -m1 '^flags' /proc/cpuinfo | cut -d: -f2- | xargs)"
lscpu > $OUT/lscpu.txt
lscpu -e > $OUT/lscpu-e.txt
cat /proc/cpuinfo > $OUT/cpuinfo.txt
dmesg > $OUT/dmesg-boot.txt
systemd-analyze blame > $OUT/blame.txt 2>&1
lsblk > $OUT/lsblk.txt
df -h / > $OUT/df-start.txt
for f in /sys/block/vda/queue/*; do [ -f "$f" ] && echo "${f##*/}: $(cat "$f" 2>/dev/null)"; done > $OUT/vda-queue.txt
cat /proc/interrupts > $OUT/interrupts-start.txt
dpkg -l > $OUT/dpkg.txt

if want sysbench-cpu; then
step sysbench-cpu
for t in 1 4; do
    for r in $(seq 1 "$(d 3 1)"); do
        v=$(timeout 300 sysbench cpu --cpu-max-prime=20000 --threads=$t --time="$(d 20 2)" run \
            | tee -a $OUT/sysbench-cpu.txt | awk '/events per second/ {print $4}')
        say sysbench_cpu t$t r$r "${v:-FAILED}" events/s
    done
done

fi

if want 7zip; then
step 7zip
for t in 1 4; do
    if [ $QUICK = 1 ]; then
        timeout 600 7z b 1 -mmt$t -md18 > $OUT/7z-mt$t.txt 2>&1
    else
        timeout 900 7z b -mmt$t > $OUT/7z-mt$t.txt 2>&1
    fi
    say 7z mt$t tot_mips "$(awk '/^Tot:/ {print $NF}' $OUT/7z-mt$t.txt)" \
        avr "[$(grep '^Avr:' $OUT/7z-mt$t.txt | tr -s ' ')]"
done

fi

if want openssl; then
step openssl
for alg in aes-256-gcm chacha20-poly1305 sha256; do
    timeout 300 openssl speed -elapsed -seconds "$(d 3 1)" -evp $alg > $OUT/openssl-$alg.txt 2>&1
    say openssl $alg 16k_bytes "$(tail -1 $OUT/openssl-$alg.txt | awk '{print $NF}')"
done

fi

if want sysbench-memory; then
step sysbench-memory
for t in 1 4; do
    for op in read write; do
        v=$(timeout 300 sysbench memory --memory-block-size=512M --memory-total-size=100000G \
                --memory-oper=$op --threads=$t --time="$(d 15 2)" run \
            | tee -a $OUT/sysbench-memory.txt | sed -n 's/.*transferred (\([0-9.]*\) MiB\/sec).*/\1/p')
        say sysbench_mem seq_$op t$t "${v:-FAILED}" MiB/s
    done
    v=$(timeout 300 sysbench memory --memory-block-size=256M --memory-total-size=100000G \
            --memory-access-mode=rnd --memory-oper=read --threads=$t --time="$(d 15 2)" run \
        | tee -a $OUT/sysbench-memory.txt | sed -n 's/.*transferred (\([0-9.]*\) MiB\/sec).*/\1/p')
    say sysbench_mem rnd_read t$t "${v:-FAILED}" MiB/s
done

fi

if want perf-bench; then
step perf-bench
PERF=$(ls /usr/lib/linux-tools/*/perf 2>/dev/null | sort -V | tail -1)
if [ -n "$PERF" ]; then
    for r in $(seq 1 "$(d 3 1)"); do
        timeout 300 $PERF bench sched pipe -l "$(d 1000000 50000)" > $OUT/perf-pipe-r$r.txt 2>&1
        say perf_pipe r$r "$(awk '/usecs\/op/ {print $1}' $OUT/perf-pipe-r$r.txt)" us/op
        timeout 300 $PERF bench sched messaging -g 20 -l "$(d 2000 100)" > $OUT/perf-messaging-r$r.txt 2>&1
        say perf_messaging r$r "$(awk '/Total time/ {print $3}' $OUT/perf-messaging-r$r.txt)" s
        timeout 300 $PERF bench syscall basic -l "$(d 10000000 1000000)" > $OUT/perf-syscall-r$r.txt 2>&1
        say perf_syscall r$r "$(awk '/usecs\/op/ {print $1}' $OUT/perf-syscall-r$r.txt)" us/op
    done
else
    say perf_bench FAILED no perf under /usr/lib/linux-tools
fi

fi

if want cyclictest; then
step cyclictest
timeout 600 cyclictest -m -q -t 4 -a -p 95 -i 1000 -D "$(d 60 5)" > $OUT/cyclictest.txt 2>&1
say cyclictest "$(sed -n 's/^T: *\([0-9]*\).*Min: *\([0-9]*\).*Avg: *\([0-9]*\).*Max: *\([0-9]*\).*/T\1 min=\2 avg=\3 max=\4;/p' $OUT/cyclictest.txt | xargs)" us

fi

if want fio; then
step fio
F=/var/lib/bench/fio.dat
SIZE=$(d 2g 256m)
rm -f $F
sync
# The file is new: its blocks are taken in the host's image as it is laid
# out, so the first write is the allocating one; every test after it
# reads and writes blocks the image has.
timeout 900 fio --name=layout --filename=$F --size=$SIZE --rw=write --bs=1m --iodepth=8 \
    --ioengine=libaio --direct=1 --end_fsync=1 --output-format=json > $OUT/fio-layout.json 2> $OUT/fio-layout.err
say fio layout_seqwrite_1m_qd8 "bw_mib=$(jq '.jobs[0].write.bw / 1024 | floor' $OUT/fio-layout.json 2>/dev/null)" \
    "secs=$(jq '.jobs[0].job_runtime / 1000' $OUT/fio-layout.json 2>/dev/null)"
# fio_job <name> <rw> <bs> <depth> <jobs> [fio options]
fio_job() {
    local name=$1 rw=$2 bs=$3 qd=$4 jobs=$5 dir
    shift 5
    timeout 600 fio --name=$name --filename=$F --size=$SIZE --rw=$rw --bs=$bs --iodepth=$qd \
        --numjobs=$jobs --ioengine=libaio --direct=1 --time_based --runtime="$(d 30 3)" \
        --ramp_time="$(d 5 1)" --group_reporting --norandommap --randrepeat=0 \
        --output-format=json "$@" > $OUT/fio-$name.json 2> $OUT/fio-$name.err
    case $rw in *read) dir=read ;; *) dir=write ;; esac
    say fio $name "$(jq -r ".jobs[0].$dir | \"iops=\(.iops|floor) bw_mib=\(.bw/1024|floor) lat_us=\(.clat_ns.mean/1000|floor) p99_us=\(.clat_ns.percentile[\"99.000000\"]/1000|floor) p999_us=\(.clat_ns.percentile[\"99.900000\"]/1000|floor)\"" $OUT/fio-$name.json 2>/dev/null || echo FAILED)"
}
fio_job randread_4k_qd1 randread 4k 1 1
fio_job randread_4k_qd32 randread 4k 32 1
fio_job randread_4k_4x16 randread 4k 16 4
fio_job randwrite_4k_qd1 randwrite 4k 1 1
fio_job randwrite_4k_qd32 randwrite 4k 32 1
fio_job read_1m_qd8 read 1m 8 1
fio_job write_1m_qd8 write 1m 8 1
fio_job randwrite_4k_fdatasync randwrite 4k 1 1 --ioengine=psync --fdatasync=1
rm -f $F
sync

fi

if want kbuild; then
step kbuild
cd /var/lib/bench
rm -rf linux-*
t0=$(now_ms)
tar xf $B/linux.tar.xz
t1=$(now_ms)
say kbuild untar_ms $((t1 - t0))
cd linux-* || say kbuild FAILED no tree
make defconfig > $OUT/kbuild-defconfig.log 2>&1
t0=$(now_ms)
if [ $QUICK = 1 ]; then
    timeout 3600 make -j4 init/ > $OUT/kbuild.log 2>&1
else
    timeout 3600 make -j4 > $OUT/kbuild.log 2>&1
fi
rc=$?
t1=$(now_ms)
say kbuild make_j4_ms $((t1 - t0)) rc=$rc
tail -30 $OUT/kbuild.log > $OUT/kbuild-tail.log
rm -f $OUT/kbuild.log
cd /var/lib/bench
t0=$(now_ms)
rm -rf linux-*
sync
t1=$(now_ms)
say kbuild rm_ms $((t1 - t0))

fi

if want pgbench; then
step pgbench
SCALE=$(d 100 5)
T=$(d 60 3)
systemctl is-active postgresql > $OUT/pg-state.txt 2>&1
t0=$(now_ms)
timeout 1800 pgbench -i -s $SCALE -q bench > $OUT/pgbench-init.txt 2>&1
rc=$?
t1=$(now_ms)
say pgbench init_s$SCALE ms=$((t1 - t0)) rc=$rc "[$(grep '^done in' $OUT/pgbench-init.txt)]"
psql -qX -d bench -c CHECKPOINT
psql -qXAt -d bench -c "select pg_size_pretty(pg_database_size('bench'))" > $OUT/pg-size.txt 2>&1
say pgbench db_size "$(cat $OUT/pg-size.txt)"
timeout 300 pgbench -S -M prepared -c 4 -j 4 -T "$(d 20 2)" bench > $OUT/pgbench-warm.txt 2>&1
# pg_run <ro|rw> <clients> <rep>
pg_run() {
    local mode=$1 c=$2 r=$3 j=$2 f opt=
    [ "$j" -gt 4 ] && j=4
    [ "$mode" = ro ] && opt=-S
    [ "$mode" = rw ] && psql -qX -d bench -c CHECKPOINT
    f=$OUT/pgbench-$mode-c$c-r$r.txt
    timeout 600 pgbench $opt -M prepared -c "$c" -j "$j" -T "$T" bench > "$f" 2>&1
    say pgbench $mode c$c r$r \
        "tps=$(sed -n 's/^tps = \([0-9.]*\) (without initial connection time)/\1/p' "$f")" \
        "lat_ms=$(sed -n 's/^latency average = \([0-9.]*\) ms/\1/p' "$f")"
}
for r in $(seq 1 "$(d 3 1)"); do
    for c in 1 4 8 16; do pg_run ro $c $r; done
    for c in 1 4 8 16; do pg_run rw $c $r; done
done
psql -qXAt -d bench -c "select pg_size_pretty(pg_database_size('bench'))" > $OUT/pg-size-end.txt 2>&1

fi

step end
cat /proc/interrupts > $OUT/interrupts-end.txt
df -h / > $OUT/df-end.txt
dmesg > $OUT/dmesg-end.txt
journalctl -b --no-pager -p warning > $OUT/journal-warnings.txt 2>&1
A=/var/lib/bench/run.tar.xz
tar -C /var/lib/bench -cJf $A run
say archive bytes "$(stat -c %s $A)" sha256 "$(sha256sum $A | cut -c1-64)"
# A line at a time, slowly enough that nos's console log -- 64 KiB a
# second off the UART's ring -- loses nothing.
con BENCH-ARCHIVE-BEGIN
base64 -w 76 $A | while IFS= read -r line; do
    printf '%s\n' "$line" > $CON
    n=$((${n:-0} + 1))
    [ $((n % 40)) = 0 ] && sleep 0.1
done
con BENCH-ARCHIVE-END
say done
sync
sleep 2
systemctl poweroff
