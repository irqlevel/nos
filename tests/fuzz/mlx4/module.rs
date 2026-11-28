//! `src/module.rs`: the module EEPROM and PTYS decoding of #17 (spec 5.11,
//! 5.12). Run by `tests/mlx4-test.py`.
#[path = "../../../src/crates/drivers/net/mlx4/src/module.rs"]
#[allow(dead_code)] // constants only the driver uses
mod module;
use module::*;

#[test]
fn chunks_stay_within_48_bytes_and_address_255() {
    let c: Vec<_> = chunks(0, 96).collect();
    assert_eq!(c, [(0, 48), (48, 48)]);
    let c: Vec<_> = chunks(128, 96).collect();
    assert_eq!(c, [(128, 48), (176, 48)]);
    assert_eq!(chunks(0, 3).collect::<Vec<_>>(), [(0, 3)]);
    // Never past device address 255 (5.11.4 item 5).
    assert_eq!(chunks(224, 48).collect::<Vec<_>>(), [(224, 32)]);
    assert_eq!(chunks(250, 20).collect::<Vec<_>>(), [(250, 6)]);
    for &(s, l) in ranges(0x03).iter().chain(ranges(0x0d)) {
        for (a, n) in chunks(s, l) {
            assert!(n >= 1 && n <= MAX_READ && a + n <= 256);
        }
    }
}

#[test]
fn identifiers_and_ranges() {
    assert_eq!(kind(0x03), Some("SFP/SFP+"));
    assert_eq!(kind(0x0c), Some("QSFP"));
    assert_eq!(kind(0x0d), Some("QSFP+"));
    assert_eq!(kind(0x11), Some("QSFP28"));
    assert_eq!(kind(0x01), None);
    assert!(ranges(0x01).is_empty());
    assert_eq!(ranges(0x03), &[(0, 96)]);
    assert_eq!(ranges(0x11), &[(0, 3), (128, 96)]);
}

#[test]
fn mad_errors() {
    assert_eq!(mad_error(0x0400), "the cable has no EEPROM (passive copper cable)");
    assert_eq!(mad_error(0x0300), "cable not connected");
    assert_eq!(mad_error(0x0712), "invalid I2C device address");
    assert_eq!(mad_error(0x2000), "unknown cable-info error");
}

fn put(e: &mut [u8; 256], at: usize, s: &[u8]) {
    e[at..at + s.len()].copy_from_slice(s);
}

#[test]
fn sfp_passive_dac() {
    let mut e = [0u8; 256];
    e[0] = 0x03;
    e[2] = 0x21;
    e[8] = 0x04;
    e[12] = 103;
    e[18] = 3;
    put(&mut e, 20, b"Mellanox        ");
    put(&mut e, 37, &[0x00, 0x02, 0xc9]);
    put(&mut e, 40, b"MC3309130-003   ");
    put(&mut e, 56, b"A1  ");
    put(&mut e, 68, b"SN0123          ");
    let mut s = String::new();
    describe(&mut s, &e).unwrap();
    let lines: Vec<_> = s.lines().collect();
    assert_eq!(lines[0], "SFP/SFP+: passive copper cable (byte 8 0x04), connector 0x21 (copper pigtail)");
    assert!(lines[1].starts_with("length 3 m (byte 18), nominal rate 10300 MBd (byte 12)"), "{}", lines[1]);
    assert_eq!(
        lines[2],
        "vendor \"Mellanox\" OUI 00:02:c9, part \"MC3309130-003\" rev \"A1\", serial \"SN0123\""
    );
    assert!(lines[3].ends_with("diagnostics absent (byte 92 0x00)"), "{}", lines[3]);

    let mut d = String::new();
    dump(&mut d, &e, 0, 96).unwrap();
    assert_eq!(d.lines().count(), 6);
    assert!(d.starts_with("eeprom 00: 03 00 21 00"));
}

#[test]
fn qsfp_copper() {
    let mut e = [0u8; 256];
    e[0] = 0x0d;
    e[1] = 0x03;
    e[130] = 0x23;
    e[131] = 0x08;
    e[146] = 1;
    e[147] = 0xb0;
    put(&mut e, 148, b"VENDOR\0\0\0\0\0\0\0\0\0\0");
    let mut s = String::new();
    describe(&mut s, &e).unwrap();
    let lines: Vec<_> = s.lines().collect();
    assert_eq!(
        lines[0],
        "QSFP+: passive copper, equalized (byte 147 0xb0), connector 0x23 (no separable connector), revision 0x03, status 0x00"
    );
    assert_eq!(lines[1], "length 1 m (byte 146), 10/40G compliance 0x08, 40GBASE-CR4 (byte 131)");
    assert!(lines[2].starts_with("vendor \"VENDOR\" "));
}

#[test]
fn unknown_identifier() {
    let mut e = [0u8; 256];
    e[0] = 0x01;
    let mut s = String::new();
    describe(&mut s, &e).unwrap();
    assert_eq!(s, "identifier 0x01: not SFP or QSFP; not decoded\n");
}

#[test]
fn link_modes() {
    assert_eq!(Modes(0).to_string(), "0x00000000 (none)");
    assert_eq!(Modes(1 << 12).to_string(), "0x00001000 (10GBASE-CR)");
    assert_eq!(
        Modes((1 << 0) | (1 << 12) | (1 << 9) | (1 << 31)).to_string(),
        "0x80001201 (1000BASE-CX-SGMII, bit 9, 10GBASE-CR, bit 31)"
    );
}

#[test]
fn ptys_register() {
    let mut r = [0u8; PTYS_LEN];
    r[0] = 0x20;
    r[1] = 1;
    r[3] = PROTO_ETH;
    r[0x0c..0x10].copy_from_slice(&0x0000_3051u32.to_be_bytes());
    r[0x18..0x1c].copy_from_slice(&0x0000_1000u32.to_be_bytes());
    r[0x24..0x28].copy_from_slice(&0x0000_1000u32.to_be_bytes());
    let mut s = String::new();
    describe_ptys(&mut s, &r).unwrap();
    let lines: Vec<_> = s.lines().collect();
    assert_eq!(lines[0], "PTYS: local port 1, protocol 0x04, AN_DISABLE_CAP 1, AN_DISABLE_ADMIN 0");
    assert_eq!(lines[1], "supported   0x00003051 (1000BASE-CX-SGMII, 10GBASE-KR, 40GBASE-CR4, 10GBASE-CR, 10GBASE-SR)");
    assert_eq!(lines[3], "operating   0x00001000 (10GBASE-CR)");
    assert_eq!(lines[4], "partner     0x00000000 (none)");
}

fn media_of(e: &[u8; 256]) -> String {
    let mut s = String::new();
    media(&mut s, e).unwrap();
    s
}

#[test]
fn media_labels_for_the_port_line() {
    // #31: SFP+ DAC, SFP+ optics, 1G SFP.
    let mut e = [0u8; 256];
    e[0] = 0x03;
    e[8] = 0x04;
    e[12] = 103;
    e[18] = 3;
    assert_eq!(media_of(&e), "10GBASE-CR (SFP+ passive DAC, 3 m)");
    e[8] = 0x08;
    assert_eq!(media_of(&e), "SFP+ active cable, 3 m");
    e[8] = 0;
    e[3] = 0x10;
    assert_eq!(media_of(&e), "10GBASE-SR (SFP+)");
    e[3] = 0x20;
    assert_eq!(media_of(&e), "10GBASE-LR (SFP+)");
    e[3] = 0;
    e[6] = 0x08;
    assert_eq!(media_of(&e), "1000BASE-T (SFP)");
    e[6] = 0;
    assert_eq!(media_of(&e), "SFP/SFP+, Ethernet type not stated");

    // QSFP+: CR4 DAC, SR4, LR4, active cable.
    let mut q = [0u8; 256];
    q[0] = 0x0d;
    q[146] = 1;
    q[131] = 0x08;
    assert_eq!(media_of(&q), "40GBASE-CR4 (QSFP+ DAC, 1 m)");
    q[131] = 0x04;
    assert_eq!(media_of(&q), "40GBASE-SR4 (QSFP+)");
    q[131] = 0x02;
    assert_eq!(media_of(&q), "40GBASE-LR4 (QSFP+)");
    q[131] = 0x01;
    assert_eq!(media_of(&q), "40G active cable (QSFP+, 1 m)");
    q[131] = 0;
    q[147] = 0xb0;
    assert_eq!(media_of(&q), "QSFP+ copper cable, 1 m");

    let mut x = [0u8; 256];
    x[0] = 0x01;
    assert_eq!(media_of(&x), "module identifier 0x01");

    assert_eq!(media_unread(0x0300), "no module");
    assert_eq!(media_unread(0x0400), "passive copper cable (no EEPROM)");
    assert_eq!(media_unread(0x0900), "module not readable");
}

#[test]
fn rates_and_the_standard_fec() {
    assert_eq!(rate(0x00), "10G");
    assert_eq!(rate(0x01), "10G");
    assert_eq!(rate(0x02), "1G");
    assert_eq!(rate(0x40), "40G");
    assert_eq!(rate(0x20), "56G");
    assert_eq!(rate(0x7f), "?");
    // #31: no IEEE FEC at any ConnectX-3 speed.
    for r in ["100M", "1G", "10G", "20G", "40G", "56G", "?", ""] {
        assert_eq!(standard_fec(r).0, "none", "{r}");
    }
    assert_eq!(standard_fec("10G").1, "standard for 10G");
    assert_eq!(standard_fec("40G").1, "standard for 40G");
    assert_eq!(standard_fec("56G").1, "no IEEE FEC at this speed");
}
