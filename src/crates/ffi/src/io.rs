unsafe extern "C" {
    pub fn outb(port: u16, data: u8);
    pub fn inb(port: u16) -> u8;
    pub fn outw(port: u16, data: u16);
    pub fn inw(port: u16) -> u16;
    pub fn outl(port: u16, data: u32);
    pub fn inl(port: u16) -> u32;
}
