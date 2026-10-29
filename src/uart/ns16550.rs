//! A 16550A without FIFOs that matter: transmits immediately and raises its
//! interrupt for received data and an empty transmitter.

use crate::{hypervisor::IrqLine, stdio};
use std::collections::VecDeque;

const DATA: u64 = 0;
const IER: u64 = 1;
const IIR: u64 = 2;
const LCR: u64 = 3;
const MCR: u64 = 4;
const LSR: u64 = 5;
const MSR: u64 = 6;
const SCR: u64 = 7;

const IER_RX: u8 = 1 << 0;
const IER_TX: u8 = 1 << 1;
const IIR_NONE: u8 = 1;
const IIR_TX: u8 = 0x02;
const IIR_RX: u8 = 0x04;
const IIR_FIFO: u8 = 0xc0;
const LCR_DLAB: u8 = 1 << 7;
const MCR_LOOP: u8 = 1 << 4;
const LSR_DATA_READY: u8 = 1 << 0;
const LSR_THR_EMPTY: u8 = 1 << 5;
const LSR_TX_EMPTY: u8 = 1 << 6;
/// Clear to send, data set ready and carrier detect.
const MSR_CONNECTED: u8 = 0xb0;

pub struct Uart {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    divisor: [u8; 2],
    /// The transmitter emptied since the guest last read IIR or wrote data.
    tx_pending: bool,
    q: VecDeque<u8>,
    irq: IrqLine,
    serial: stdio::Sink,
}

impl Uart {
    pub fn new(irq: IrqLine, serial: stdio::Sink) -> Self {
        Self {
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            divisor: [1, 0],
            tx_pending: false,
            q: VecDeque::new(),
            irq,
            serial,
        }
    }

    pub fn enqueue(&mut self, value: u8) {
        self.q.push_back(value);
        self.sync_irq();
    }

    fn interrupt(&self) -> u8 {
        if self.ier & IER_RX != 0 && !self.q.is_empty() {
            IIR_RX
        } else if self.ier & IER_TX != 0 && self.tx_pending {
            IIR_TX
        } else {
            IIR_NONE
        }
    }

    fn sync_irq(&mut self) {
        self.irq.set(self.interrupt() != IIR_NONE);
    }

    pub fn read(&mut self, offset: u64) -> u32 {
        let dlab = self.lcr & LCR_DLAB != 0;
        let value = match offset {
            DATA if dlab => self.divisor[0],
            IER if dlab => self.divisor[1],
            DATA => self.q.pop_front().unwrap_or(0),
            IER => self.ier,
            IIR => {
                let interrupt = self.interrupt();
                if interrupt == IIR_TX {
                    self.tx_pending = false;
                }
                interrupt | IIR_FIFO
            }
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => LSR_THR_EMPTY | LSR_TX_EMPTY | (!self.q.is_empty() as u8 * LSR_DATA_READY),
            MSR => MSR_CONNECTED,
            SCR => self.scr,
            _ => 0,
        };

        self.sync_irq();
        value as u32
    }

    pub fn write(&mut self, offset: u64, value: u32) {
        let value = value as u8;
        let dlab = self.lcr & LCR_DLAB != 0;
        match offset {
            DATA if dlab => self.divisor[0] = value,
            IER if dlab => self.divisor[1] = value,
            DATA => {
                if self.mcr & MCR_LOOP != 0 {
                    self.q.push_back(value);
                } else {
                    self.serial.write(value);
                }
                self.tx_pending = true;
            }
            IER => {
                // Enabling the transmitter interrupt reports the empty transmitter at once.
                self.tx_pending |= value & IER_TX != 0 && self.ier & IER_TX == 0;
                self.ier = value & 0x0f;
            }
            LCR => self.lcr = value,
            MCR => self.mcr = value,
            SCR => self.scr = value,
            _ => {}
        }

        self.sync_irq();
    }
}
