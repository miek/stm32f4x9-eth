use core::mem;
use core::default::Default;
use core::fmt::Write;
use cortex_m_semihosting::hio;
use alloc::Vec;
use alloc::allocator::{Alloc, Layout};
use alloc::heap::Heap;
use board::ETHERNET_DMA;
use volatile_register::RW;

use super::buffer::Buffer;


/// Owned by DMA engine
const RXDESC_0_OWN: u32 = 1 << 31;
/// First descriptor
const RXDESC_0_FS: u32 = 1 << 9;
/// Last descriptor
const RXDESC_0_LS: u32 = 1 << 8;
/// Error summary
const RXDESC_0_ES: u32 = 1 << 15;
/// Frame length
const RXDESC_0_FL_MASK: u32 = 0x3FFF;
const RXDESC_0_FL_SHIFT: usize = 16;

const RXDESC_1_RBS_SHIFT: usize = 0;
const RXDESC_1_RBS_MASK: u32 = 0x0fff << RXDESC_1_RBS_SHIFT;
/// Second address chained
const RXDESC_1_RCH: u32 = 1 << 14;
/// End Of Ring
const RXDESC_1_RER: u32 = 1 << 15;

#[repr(C)]
struct RxDescriptor {
    rdesc: &'static mut [RW<u32>; 4],
}

impl Default for RxDescriptor {
    fn default() -> Self {
        let mut this = Self::new();
        this.write(0, 0);
        this.write(1, RXDESC_1_RCH);
        this.write(2, 0);
        this.write(3, 0);
        this
    }
}

impl Drop for RxDescriptor {
    fn drop(&mut self) {
        unsafe {
            Heap.dealloc(self.rdesc.as_mut_ptr() as *mut u8, Self::memory_layout())
        }
    }
}

impl RxDescriptor {
    fn memory_layout() -> Layout {
        Layout::from_size_align(4 * 4, super::ALIGNMENT)
            .unwrap()
    }

    fn new() -> Self {
        let mem = unsafe {
            Heap.alloc(Self::memory_layout())
        }.expect("alloc with memory_layout") as *mut [u32; 4];

        RxDescriptor {
            rdesc: unsafe { &mut *(mem as *mut [RW<u32>; 4]) },
        }
    }

    fn as_raw_ptr(&self) -> *const u8 {
        self.rdesc.as_ptr() as *const u8
    }

    fn read(&self, i: usize) -> u32 {
        self.rdesc[i].read()
    }

    fn write(&mut self, i: usize, data: u32) {
        unsafe { self.rdesc[i].write(data) }
    }

    fn modify<F>(&mut self, i: usize, f: F)
        where F: (FnOnce(u32) -> u32) {

        unsafe { self.rdesc[i].modify(f) }
    }

    /// Is owned by the DMA engine?
    pub fn is_owned(&self) -> bool {
        (self.read(0) & RXDESC_0_OWN) == RXDESC_0_OWN
    }

    pub fn set_owned(&mut self) {
        self.modify(0, |w| w | RXDESC_0_OWN);
    }

    pub fn has_error(&self) -> bool {
        (self.read(0) & RXDESC_0_ES) == RXDESC_0_ES
    }

    /// Descriptor contains first buffer of frame
    pub fn is_first(&self) -> bool {
        (self.read(0) & RXDESC_0_FS) == RXDESC_0_FS
    }

    /// Descriptor contains last buffers of frame
    pub fn is_last(&self) -> bool {
        (self.read(0) & RXDESC_0_LS) == RXDESC_0_LS
    }

    pub fn set_buffer1(&mut self, buffer: *const u8, len: usize) {
        self.write(2, buffer as u32);
        self.modify(1, |w| {
            (w & !RXDESC_1_RBS_MASK) |
            ((len as u32) << RXDESC_1_RBS_SHIFT)
        });
    }

    // points to next descriptor (RCH)
    pub fn set_buffer2(&mut self, buffer: *const u8) {
        self.write(3, buffer as u32);
    }

    pub fn set_end_of_ring(&mut self) {
        self.modify(1, |w| w | RXDESC_1_RER);
    }
}

struct RxRingEntry {
    desc: RxDescriptor,
    buffer: Buffer,
}

impl RxRingEntry {
    fn new(capacity: usize) -> Self {
        let mut desc = RxDescriptor::default();
        let buffer = Buffer::new(capacity);
        desc.set_buffer1(buffer.as_ptr(), buffer.capacity());
        desc.set_owned();
        RxRingEntry {
            desc: desc,
            buffer,
        }
    }

    // Used to chain all buffers in the ring on start
    pub fn set_next_buffer(&mut self, next: Option<&RxRingEntry>) {
        match next {
            Some(next_buffer) => {
                let ptr = next_buffer.desc.as_raw_ptr();
                self.desc.set_buffer2(ptr);
            },
            // For the last in the ring
            None => {
                self.desc.set_buffer2(0 as *const u8);
                self.desc.set_end_of_ring();
            },
        }
    }

    fn take_received(&mut self) -> Option<Buffer> {
        match self.desc.is_owned() {
            true => None,
            false if self.desc.has_error() => {
                let mut stderr = hio::hstderr().unwrap();
                writeln!(stderr, "Ethernet error: skipping error frame").unwrap();
                self.desc.set_owned();
                None
            },
            false if self.desc.is_first() && self.desc.is_last() => {
                // Switch old with new
                let new_buffer = Buffer::new(self.buffer.capacity());
                let mut pkt_buffer = mem::replace(&mut self.buffer, new_buffer);
                // Truncate received pkt to reported length
                let frame_length = ((self.desc.read(0) >> RXDESC_0_FL_SHIFT) & RXDESC_0_FL_MASK) as usize;
                pkt_buffer.set_len(frame_length);
                // TODO: obtain ethernet frame type (RDESC_1_FT)

                self.desc.set_buffer1(self.buffer.as_ptr(), self.buffer.capacity());
                self.desc.set_owned();

                Some(pkt_buffer)
            },
            false => {
                let mut stderr = hio::hstderr().unwrap();
                writeln!(stderr, "Ethernet error: skipping truncated frame bufs (FS={:?} LS={:?})",
                         self.desc.is_first(), self.desc.is_last()).unwrap();
                self.desc.set_owned();
                None
            },
        }
    }
}

/// Rx DMA state
pub struct RxRing {
    buffer_size: usize,
    buffers: Vec<RxRingEntry>,
    next_entry: usize,
}

impl RxRing {
    /// Allocate
    pub fn new(buffer_size: usize) -> Self {
        RxRing {
            buffer_size,
            buffers: Vec::new(),
            next_entry: 0,
        }
    }

    /// Setup the DMA engine (**required**)
    pub fn start(&mut self, ring_length: usize, eth_dma: &ETHERNET_DMA) {
        let mut buffers = mem::replace(&mut self.buffers, Vec::with_capacity(ring_length));
        // Grow ring if necessary
        let additional = ring_length.saturating_sub(buffers.len());
        if additional > 0 {
            self.buffers.reserve(additional);
            while buffers.len() < ring_length {
                let buffer = RxRingEntry::new(self.buffer_size);
                buffers.push(buffer);
            }
        }

        // Setup ring from `buffers` back into `self.buffers`
        let mut previous: Option<RxRingEntry> = None;
        for buffer in buffers.into_iter() {
            previous.take().map(|mut previous| {
                previous.set_next_buffer(Some(&buffer));
                self.buffers.push(previous);
            });
            previous = Some(buffer);
        }
        previous.map(|mut previous| {
            previous.set_next_buffer(None);
            self.buffers.push(previous);
        });

        self.next_entry = 0;
        let ring_ptr = self.buffers[0].desc.as_raw_ptr();
        // Register RxDescriptor
        eth_dma.dmardlar.write(|w| unsafe { w.srl().bits(ring_ptr as u32) });

        // Start receive
        eth_dma.dmaomr.modify(|_, w| w.sr().set_bit());

        self.demand_poll(eth_dma);
    }

    /// Demand that the DMA engine polls the current `RxDescriptor`
    /// (when in `RunningState::Stopped`.)
    pub fn demand_poll(&self, eth_dma: &ETHERNET_DMA) {
        eth_dma.dmarpdr.write(|w| unsafe { w.rpd().bits(1) });
    }

    /// Get current `RunningState`
    pub fn running_state(&self, eth_dma: &ETHERNET_DMA) -> RunningState {
        match eth_dma.dmasr.read().rps().bits() {
            //  Reset or Stop Receive Command issued
            0b000 => RunningState::Stopped,
            //  Fetching receive transfer descriptor
            0b001 => RunningState::Running,
            //  Waiting for receive packet
            0b011 => RunningState::Running,
            //  Receive descriptor unavailable
            0b100 => RunningState::Stopped,
            //  Closing receive descriptor
            0b101 => RunningState::Running,
            //  Transferring the receive packet data from receive buffer to host memory
            0b111 => RunningState::Running,
            _ => RunningState::Unknown,
        }
    }

    /// Receive the next packet (if any is ready), or return `None`
    /// immediately.
    pub fn recv_next(&mut self, eth_dma: &ETHERNET_DMA) -> Option<Buffer> {
        let result = self.buffers[self.next_entry]
            .take_received()
            .map(|pkt| {
                self.next_entry += 1;
                if self.next_entry >= self.buffers.len() {
                    self.next_entry = 0;
                }

                pkt
            });

        if ! self.running_state(eth_dma).is_running() {
            self.demand_poll(eth_dma);
        }

        result
    }
}

/// Running state of the `RxRing`
#[derive(PartialEq, Eq, Debug)]
pub enum RunningState {
    Unknown,
    Stopped,
    Running,
}

impl RunningState {
    /// whether self equals to `RunningState::Running`
    pub fn is_running(&self) -> bool {
        *self == RunningState::Running
    }
}
