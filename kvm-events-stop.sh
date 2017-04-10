#!/bin/bash -xv
echo 0 > /sys/kernel/debug/tracing/tracing_on
echo '' > /sys/kernel/debug/tracing/trace
echo 0 > /sys/kernel/debug/tracing/events/kvm/enable
