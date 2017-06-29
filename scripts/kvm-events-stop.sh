#!/bin/bash -xv
cat /sys/kernel/debug/tracing/trace > trace.out
echo 0 > /sys/kernel/debug/tracing/tracing_on
echo '' > /sys/kernel/debug/tracing/trace
echo 0 > /sys/kernel/debug/tracing/events/kvm/enable
