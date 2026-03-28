#![allow(dead_code)]

/* NVMe 1.4 register offsets, command/completion structures, and constants.
 * All offsets are byte offsets from BAR0. */

/* --- Controller registers (BAR0) --- */
pub const REG_CAP:  usize = 0x00;  /* Controller Capabilities (8 bytes) */
pub const REG_VS:   usize = 0x08;  /* Version (4 bytes) */
pub const REG_CC:   usize = 0x14;  /* Controller Configuration (4 bytes) */
pub const REG_CSTS: usize = 0x1C;  /* Controller Status (4 bytes) */
pub const REG_AQA:  usize = 0x24;  /* Admin Queue Attributes (4 bytes) */
pub const REG_ASQ:  usize = 0x28;  /* Admin Submission Queue Base (8 bytes) */
pub const REG_ACQ:  usize = 0x30;  /* Admin Completion Queue Base (8 bytes) */

/* CC register fields */
pub const CC_EN:          u32 = 1 << 0;    /* Enable */
pub const CC_CSS_NVM:     u32 = 0 << 4;    /* I/O Command Set: NVM */
pub const CC_MPS_4K:      u32 = 0 << 7;    /* Memory Page Size: 4096 (MPS=0 -> 2^(12+0)) */
pub const CC_AMS_RR:      u32 = 0 << 11;   /* Arbitration: Round Robin */
pub const CC_IOSQES:      u32 = 6 << 16;   /* I/O SQ Entry Size: 2^6 = 64 bytes */
pub const CC_IOCQES:      u32 = 4 << 20;   /* I/O CQ Entry Size: 2^4 = 16 bytes */

/* CSTS register fields */
pub const CSTS_RDY:  u32 = 1 << 0;  /* Controller Ready */
pub const CSTS_FATAL: u32 = 1 << 1; /* Controller Fatal Status */

/* CAP register fields (64-bit) */
pub const CAP_DSTRD_SHIFT: u32 = 32;  /* Doorbell Stride field bit offset */
pub const CAP_DSTRD_MASK:  u64 = 0xF; /* 4-bit field */
pub const CAP_MQES_MASK:   u64 = 0xFFFF; /* Max Queue Entries Supported (0-based) */
pub const CAP_TO_SHIFT:    u32 = 24;  /* Timeout field (units of 500ms) in low 32 bits */
pub const CAP_TO_MASK:     u64 = 0xFF;

/* Doorbell base offset within BAR0 */
pub const DB_BASE: usize = 0x1000;

/* Queue entry sizes */
pub const SQE_SIZE: usize = 64;
pub const CQE_SIZE: usize = 16;

/* Queue depths */
pub const ADMIN_QUEUE_DEPTH: usize = 16;
pub const IO_QUEUE_DEPTH:    usize = 64;

/* Minimum controller ready/disable timeout when CAP.TO = 0 (ms).
 * The NVMe spec treats TO=0 as "unspecified", not "instant". */
pub const MIN_TIMEOUT_MS: u64 = 5_000;

/* NVMe admin command opcodes */
pub const OPC_DELETE_IO_SQ: u8 = 0x00;
pub const OPC_CREATE_IO_SQ: u8 = 0x01;
pub const OPC_DELETE_IO_CQ: u8 = 0x04;
pub const OPC_CREATE_IO_CQ: u8 = 0x05;
pub const OPC_IDENTIFY:     u8 = 0x06;

/* NVMe I/O command opcodes */
pub const OPC_FLUSH: u8 = 0x00;
pub const OPC_WRITE: u8 = 0x01;
pub const OPC_READ:  u8 = 0x02;

/* Identify CNS values */
pub const CNS_CONTROLLER: u32 = 0x01;
pub const CNS_NAMESPACE:  u32 = 0x00;

/* CreateCQ/CreateSQ flags */
pub const CQ_IEN:        u16 = 1 << 1;  /* Interrupts Enabled */
pub const CQ_PC:         u16 = 1 << 0;  /* Physically Contiguous */
pub const SQ_PC:         u16 = 1 << 0;  /* Physically Contiguous */

/* Completion status: Status Code Type */
pub const SCT_GENERIC: u8 = 0x0;
pub const SC_SUCCESS:  u8 = 0x0;

/* --- NVMe Submission Queue Entry (64 bytes) --- */
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SubmissionEntry {
    pub cdw0:  u32,  /* opcode[7:0], fuse[9:8], psdt[15:14], cid[31:16] */
    pub nsid:  u32,
    pub cdw2:  u32,
    pub cdw3:  u32,
    pub mptr:  u64,  /* Metadata Pointer */
    pub prp1:  u64,  /* PRP Entry 1 */
    pub prp2:  u64,  /* PRP Entry 2 */
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}

impl SubmissionEntry {
    pub fn new(opcode: u8, cid: u16) -> Self {
        let mut e = Self::default();
        e.cdw0 = (opcode as u32) | ((cid as u32) << 16);
        e
    }

    pub fn set_prps(&mut self, prp1: u64, prp2: u64) {
        self.prp1 = prp1;
        self.prp2 = prp2;
    }
}

/* --- NVMe Completion Queue Entry (16 bytes) --- */
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CompletionEntry {
    pub dw0: u32,  /* Command specific */
    pub dw1: u32,  /* Reserved */
    pub sq_head: u16,
    pub sq_id:   u16,
    pub cid:     u16,
    pub status:  u16,  /* phase[0], status[15:1] */
}

impl CompletionEntry {
    pub fn phase(&self) -> bool {
        self.status & 1 != 0
    }

    /* Returns the 15-bit status field (SC + SCT). 0 = success. */
    pub fn status_code(&self) -> u16 {
        self.status >> 1
    }
}

/* --- Identify Controller response (only fields we use, 4096 bytes total) ---
 * Layout per NVMe 1.4 spec §5.15.2.1:
 *   offset  0: VID (2), SSVID (2)
 *   offset  4: SN[20]
 *   offset 24: MN[40]
 *   offset 64: FR[8]
 *   offset 72: RAB (1), IEEE[3], CMIC (1)
 *   offset 77: MDTS (1)   <- last field we read
 *   offset 78..4095: remainder (4018 bytes)
 * Total: 78 + 4018 = 4096 bytes. */
#[repr(C)]
pub struct IdentifyController {
    pub vid:       u16,
    pub ssvid:     u16,
    pub sn:        [u8; 20],
    pub mn:        [u8; 40],
    pub fr:        [u8; 8],
    pub rab:       u8,
    pub ieee:      [u8; 3],
    pub cmic:      u8,
    pub mdts:      u8,   /* Max Data Transfer Size (2^n pages, 0=unlimited) */
    _pad:          [u8; 4018],
}

/* --- Identify Namespace response (only fields we use, 4096 bytes total) --- */
#[repr(C)]
pub struct IdentifyNamespace {
    pub nsze:  u64,  /* Namespace Size (total blocks) */
    pub ncap:  u64,  /* Namespace Capacity */
    pub nuse:  u64,  /* Namespace Utilization */
    pub nsfeat: u8,
    pub nlbaf:  u8,  /* Number of LBA Formats (0-based) */
    pub flbas:  u8,  /* Formatted LBA Size: bits[3:0] = lbaf index */
    _pad:       [u8; 4069],
}

/* LBA Format entry (embedded in IdentifyNamespace at offset 128) */
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LbaFormat {
    pub ms:   u16,  /* Metadata Size */
    pub lbads: u8,  /* LBA Data Size: sector size = 2^lbads */
    pub rp:   u8,   /* Relative Performance */
}
