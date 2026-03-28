extern "C" {
    pub fn Outb(port: u16, data: u8);
    pub fn Inb(port: u16) -> u8;
    pub fn Outw(port: u16, data: u16);
    pub fn Inw(port: u16) -> u16;
    pub fn Out(port: u16, data: u32);
    pub fn In(port: u16) -> u32;
    pub fn ReadMsr(msr: u32) -> u64;
    pub fn WriteMsr(msr: u32, value: u64);
}
