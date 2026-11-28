pub fn stall(duration: core::time::Duration) {
    kcore::time::delay_us(duration.as_micros().min(u128::from(u64::MAX)) as u64);
}
