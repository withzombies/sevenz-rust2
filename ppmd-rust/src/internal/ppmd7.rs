mod decoder;
mod encoder;
mod range_coding;

use std::{
    alloc::{Layout, alloc, dealloc},
    io::{Read, Write},
    ptr::{NonNull, write_bytes},
};

pub(crate) use range_coding::{RangeDecoder, RangeEncoder};

use super::{PPMD_BIN_SCALE, PPMD_NUM_INDEXES, PPMD_PERIOD_BITS};
use crate::Error;

const MAX_FREQ: u8 = 124;
const UNIT_SIZE: isize = 12;
const K_TOP_VALUE: u32 = 1 << 24;
const EMPTY_NODE: u16 = 0;

static K_EXP_ESCAPE: [u8; 16] = [25, 14, 9, 7, 5, 5, 4, 4, 4, 3, 3, 3, 2, 2, 2, 2];

static K_INIT_BIN_ESC: [u16; 8] = [
    0x3CDD, 0x1F3F, 0x59BF, 0x48F3, 0x64A1, 0x5ABC, 0x6632, 0x6051,
];

#[derive(Copy, Clone, Default)]
#[repr(C, packed)]
struct See {
    summ: u16,
    shift: u8,
    count: u8,
}

impl See {
    fn update(&mut self) {
        if self.shift < 7 && {
            self.count -= 1;
            self.count == 0
        } {
            self.summ <<= 1;
            let fresh = self.shift;
            self.shift += 1;
            self.count = (3 << fresh) as u8;
        }
    }
}

enum SeeSource {
    Dummy,
    Table(usize, usize),
}

#[derive(Copy, Clone)]
#[repr(C, packed)]
struct State {
    symbol: u8,
    freq: u8,
    successor_0: u16,
    successor_1: u16,
}

#[derive(Copy, Clone)]
#[repr(C, packed)]
struct State2 {
    symbol: u8,
    freq: u8,
}

#[derive(Copy, Clone)]
#[repr(C, packed)]
struct State4 {
    successor_0: u16,
    successor_1: u16,
}

#[derive(Copy, Clone)]
#[repr(C, packed)]
union Union2 {
    summ_freq: u16,
    state2: State2,
}

#[derive(Copy, Clone)]
#[repr(C, packed)]
union Union4 {
    stats: u32,
    state4: State4,
}

#[derive(Copy, Clone)]
#[repr(C, packed)]
struct Context {
    num_stats: u16,
    union2: Union2,
    union4: Union4,
    suffix: u32,
}

#[derive(Copy, Clone)]
#[repr(C)]
struct Node {
    /// Must be at offset 0 as Context.num_stats. Stamp = 0 means free.
    stamp: u16,
    nu: u16,
    /// Must be at offset >= 4.
    next: u32,
    prev: u32,
}

#[derive(Copy, Clone)]
#[repr(C)]
union NodeUnion {
    node: Node,
    next_ref: u32,
}

pub(crate) struct Pppmd7<RC> {
    min_context: NonNull<Context>,
    max_context: NonNull<Context>,
    found_state: NonNull<State>,
    order_fall: u32,
    init_esc: u32,
    prev_success: u32,
    max_order: u32,
    hi_bits_flag: u32,
    run_length: i32,
    init_rl: i32,
    size: u32,
    glue_count: u32,
    align_offset: u32,
    lo_unit: NonNull<u8>,
    hi_unit: NonNull<u8>,
    text: NonNull<u8>,
    units_start: NonNull<u8>,
    index2units: [u8; 40],
    units2index: [u8; 128],
    free_list: [u32; 38],
    ns2bs_index: [u8; 256],
    ns2index: [u8; 256],
    exp_escape: [u8; 16],
    dummy_see: See,
    see: [[See; 16]; 25],
    bin_summ: [[u16; 64]; 128],
    memory_ptr: NonNull<u8>,
    memory_layout: Layout,
    rc: RC,
}

impl<RC> Drop for Pppmd7<RC> {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.memory_ptr.as_ptr(), self.memory_layout);
        }
    }
}

impl<RC> Pppmd7<RC> {
    fn new(rc: RC, order: u32, mem_size: u32) -> Result<Pppmd7<RC>, Error> {
        let mut units2index = [0u8; 128];
        let mut index2units = [0u8; 40];
        let mut k = 0;

        for i in 0..PPMD_NUM_INDEXES {
            let step: u32 = if i >= 12 { 4 } else { (i >> 2) + 1 };
            for _ in 0..step {
                units2index[k as usize] = i as u8;
                k += 1;
            }
            index2units[i as usize] = k as u8;
        }

        let mut ns2bs_index = [0u8; 256];
        ns2bs_index[0] = (0 << 1) as u8;
        ns2bs_index[1] = (1 << 1) as u8;
        ns2bs_index[2..11].fill((2 << 1) as u8);
        ns2bs_index[11..256].fill((3 << 1) as u8);

        let mut ns2index = [0u8; 256];
        for i in 0..3 {
            ns2index[i as usize] = i as u8;
        }

        let mut m = 3;
        let mut k = 1;
        for i in 3..256 {
            ns2index[i as usize] = m as u8;
            k -= 1;
            if k == 0 {
                m += 1;
                k = m - 2;
            }
        }

        let align_offset = (4u32.wrapping_sub(mem_size)) & 3;
        let total_size = (align_offset + mem_size) as usize;

        let memory_layout = Layout::from_size_align(total_size, align_of::<usize>())
            .expect("Failed to create memory layout");

        let memory_ptr = unsafe {
            let Some(memory_ptr) = NonNull::new(alloc(memory_layout)) else {
                return Err(Error::InternalError("Failed to allocate memory for PPMD7"));
            };

            write_bytes(memory_ptr.as_ptr(), 0, total_size);
            memory_ptr
        };

        // We set the NonNull pointer to the start of the allocated memory as a dummy value.
        // We se them in the 'restart_model()' function right after this.
        let mut ppmd = Pppmd7 {
            min_context: memory_ptr.cast(),
            max_context: memory_ptr.cast(),
            found_state: memory_ptr.cast(),
            order_fall: 0,
            init_esc: 0,
            prev_success: 0,
            max_order: order,
            hi_bits_flag: 0,
            run_length: 0,
            init_rl: 0,
            size: mem_size,
            glue_count: 0,
            align_offset,
            lo_unit: memory_ptr,
            hi_unit: memory_ptr,
            text: memory_ptr,
            units_start: memory_ptr,
            units2index,
            index2units,
            ns2bs_index,
            ns2index,
            exp_escape: K_EXP_ESCAPE,
            dummy_see: See {
                summ: 0,
                shift: 0,
                count: 0,
            },
            see: [[See::default(); 16]; 25],
            free_list: [0; PPMD_NUM_INDEXES as usize],
            bin_summ: [[0; 64]; 128],
            memory_ptr,
            memory_layout,
            rc,
        };

        unsafe { ppmd.restart_model() };

        Ok(ppmd)
    }

    unsafe fn ptr_of_offset(&self, offset: isize) -> NonNull<u8> {
        unsafe { self.memory_ptr.offset(offset) }
    }

    unsafe fn offset_for_ptr(&self, ptr: NonNull<u8>) -> u32 {
        unsafe {
            let offset = ptr.offset_from(self.memory_ptr);
            u32::try_from(offset).expect("Failed to convert ptr to offset")
        }
    }

    unsafe fn insert_node(&mut self, node: NonNull<u8>, index: u32) {
        unsafe {
            *node.cast::<u32>().as_mut() = self.free_list[index as usize];
            self.free_list[index as usize] = self.offset_for_ptr(node);
        }
    }

    unsafe fn remove_node(&mut self, index: u32) -> NonNull<u8> {
        unsafe {
            let node = self
                .ptr_of_offset(self.free_list[index as usize] as isize)
                .cast::<u32>();
            self.free_list[index as usize] = *node.as_ref();
            node.cast()
        }
    }

    unsafe fn split_block(&mut self, mut ptr: NonNull<u8>, old_index: u32, new_index: u32) {
        unsafe {
            let nu = (self.index2units[old_index as usize] as u32)
                - (self.index2units[new_index as usize] as u32);
            ptr = ptr.offset(self.index2units[new_index as usize] as isize * UNIT_SIZE);
            let mut i = self.units2index[(nu as usize) - 1] as u32;

            if self.index2units[i as usize] as u32 != nu {
                i -= 1;
                let k = self.index2units[i as usize] as u32;
                self.insert_node(ptr.offset(k as isize * UNIT_SIZE), nu - k - 1);
            }

            self.insert_node(ptr, i);
        }
    }

    unsafe fn glue_free_blocks(&mut self) {
        unsafe {
            // We use first u16 field of 12-bytes unsigned integer as record type stamp.
            // State   { symbol: u8, freq: u8, .. : freq != 0
            // Context { num_stats: u16, ..       : num_stats != 0
            // Node    { stamp: u16               : stamp == 0 for free record
            //                                    : Stamp == 1 for head record and guard
            // Last 12-bytes unsigned int in array is always containing 12-bytes order-0 Context
            // record.

            let mut n = 0;
            self.glue_count = 255;

            // We set guard node at LoUnit.
            if self.lo_unit != self.hi_unit {
                self.lo_unit.cast::<Node>().as_mut().stamp = 1;
            }

            // Create list of free blocks. We still need one additional list walk pass before glue.
            let mut i = 0;
            while i < PPMD_NUM_INDEXES {
                let nu = self.index2units[i as usize] as u16;
                let mut next = self.free_list[i as usize];
                self.free_list[i as usize] = 0;
                while next != 0 {
                    // Don't change the order of the following commands:
                    let un = self
                        .ptr_of_offset(next as isize)
                        .cast::<Node>()
                        .cast::<NodeUnion>()
                        .as_mut();
                    let tmp = next;
                    next = un.next_ref;
                    un.node.stamp = EMPTY_NODE;
                    un.node.nu = nu;
                    un.node.next = n;
                    n = tmp;
                }
                i += 1;
            }

            let mut head = n;

            // Glue and fill must walk the list in same direction.
            self.glue_blocks(n, &mut head);
            self.fill_list(head);
        }
    }

    ///  Glue free blocks.
    unsafe fn glue_blocks(&mut self, mut n: u32, head: &mut u32) {
        unsafe {
            let mut prev = head;
            while n != 0 {
                let mut node = self.ptr_of_offset(n as isize).cast::<Node>();
                let mut nu = node.as_ref().nu as u32;
                n = node.as_ref().next;
                if nu == 0 {
                    *prev = n;
                } else {
                    prev = &mut node.as_mut().next;
                    loop {
                        let mut node2 = node.offset(nu as isize);
                        nu += node2.as_ref().nu as u32;
                        if node2.as_ref().stamp != EMPTY_NODE || nu >= 0x10000 {
                            break;
                        }
                        node.as_mut().nu = nu as u16;
                        node2.as_mut().nu = 0;
                    }
                }
            }
        }
    }

    /// Fill lists of free blocks.
    unsafe fn fill_list(&mut self, head: u32) {
        unsafe {
            let mut n = head;
            while n != 0 {
                let mut node = self.ptr_of_offset(n as isize).cast::<Node>();
                let mut nu = node.as_ref().nu as u32;

                n = node.as_ref().next;
                if nu == 0 {
                    continue;
                }
                while nu > 128 {
                    self.insert_node(node.cast(), PPMD_NUM_INDEXES - 1);
                    nu -= 128;
                    node = node.offset(128);
                }

                let mut index = self.units2index[(nu as usize) - 1] as u32;
                if self.index2units[index as usize] as u32 != nu {
                    index -= 1;
                    let k = self.index2units[index as usize] as u32;
                    self.insert_node(node.offset(k as isize).cast(), nu - k - 1);
                }
                self.insert_node(node.cast(), index);
            }
        }
    }

    #[inline(never)]
    unsafe fn alloc_units_rare(&mut self, index: u32) -> Option<NonNull<u8>> {
        unsafe {
            if self.glue_count == 0 {
                self.glue_free_blocks();
                if self.free_list[index as usize] != 0 {
                    return Some(self.remove_node(index));
                }
            }

            let mut i = index;

            loop {
                i += 1;
                if i == PPMD_NUM_INDEXES {
                    let num_bytes = self.index2units[index as usize] as u32 * UNIT_SIZE as u32;
                    let us = self.units_start;
                    self.glue_count -= 1;
                    return if us.offset_from(self.text) > num_bytes as isize {
                        self.units_start = us.offset(-(num_bytes as isize));
                        Some(self.units_start)
                    } else {
                        None
                    };
                }
                if self.free_list[i as usize] != 0 {
                    break;
                }
            }

            let block = self.remove_node(i);
            self.split_block(block, i, index);
            Some(block)
        }
    }

    unsafe fn alloc_units(&mut self, index: u32) -> Option<NonNull<u8>> {
        unsafe {
            if self.free_list[index as usize] != 0 {
                return Some(self.remove_node(index));
            }
            let num_bytes = self.index2units[index as usize] as u32 * UNIT_SIZE as u32;
            let lo = self.lo_unit;
            if self.hi_unit.offset_from(lo) as u32 >= num_bytes {
                self.lo_unit = lo.offset(num_bytes as isize);
                return Some(lo);
            }

            self.alloc_units_rare(index)
        }
    }

    unsafe fn set_successor(p: &mut State, v: u32) {
        p.successor_0 = v as u16;
        p.successor_1 = (v >> 16) as u16;
    }

    unsafe fn get_successor(&mut self, s: NonNull<State>) -> NonNull<Context> {
        unsafe {
            self.ptr_of_offset(
                (s.as_ref().successor_0 as u32 | (s.as_ref().successor_1 as u32) << 16) as isize,
            )
            .cast()
        }
    }

    #[inline(never)]
    unsafe fn restart_model(&mut self) {
        unsafe {
            self.free_list = [0; 38];

            self.text = self.ptr_of_offset(self.align_offset as isize);
            self.hi_unit = self.text.offset(self.size as isize);
            self.units_start = self
                .hi_unit
                .offset(-(self.size as isize / 8 / UNIT_SIZE * 7 * UNIT_SIZE));
            self.lo_unit = self.units_start;
            self.glue_count = 0;

            self.order_fall = self.max_order;
            self.init_rl = -(if self.max_order < 12 {
                self.max_order as i32
            } else {
                12
            }) - 1;
            self.run_length = self.init_rl;
            self.prev_success = 0;

            self.hi_unit = self.hi_unit.offset(-UNIT_SIZE);
            let mut mc = self.hi_unit.cast::<Context>();
            let s = self.lo_unit.cast::<State>();

            self.lo_unit = self.lo_unit.offset((256 / 2) * UNIT_SIZE);
            self.min_context = mc;
            self.max_context = mc;
            self.found_state = s;

            {
                let mc = mc.as_mut();
                mc.num_stats = 256;
                mc.union2.summ_freq = (256 + 1) as u16;
                mc.union4.stats = self.offset_for_ptr(s.cast());
                mc.suffix = 0;
            }

            (0..256).for_each(|i| {
                let s = s.offset(i).as_mut();
                s.symbol = i as u8;
                s.freq = 1;
                s.successor_0 = 0;
                s.successor_1 = 0;
            });

            (0..128).for_each(|i| {
                (0..8).for_each(|k| {
                    let val = PPMD_BIN_SCALE - (K_INIT_BIN_ESC[k] as u32) / (i as u32 + 2);

                    (0..64).step_by(8).for_each(|m| {
                        self.bin_summ[i][k + m] = val as u16;
                    });
                });
            });

            (0..25).for_each(|i| {
                let summ = (5 * i as u32 + 10) << (PPMD_PERIOD_BITS - 4);
                (0..16).for_each(|k| {
                    let s = &mut self.see[i][k];
                    s.summ = summ as u16;
                    s.shift = (PPMD_PERIOD_BITS - 4) as u8;
                    s.count = 4;
                });
            });

            self.dummy_see.summ = 0;
            self.dummy_see.shift = PPMD_PERIOD_BITS as u8;
            self.dummy_see.count = 64;
        }
    }

    /// It's called when `found_state.successor` is RAW-successor, that is the link to position
    /// in raw text. So we create Context records and write the links to `found_state.successor`
    /// and to identical RAW-successors in suffix contexts of `min_context`.
    ///
    /// The function returns:
    ///
    /// if (`porder_fall` == 0) then `min_context` is already at MAX order,
    ///   { return pointer to new or existing context of same MAX order }
    /// else
    ///   { return pointer to new real context that will be (order+1) in comparison with `min_context` }
    ///
    /// also it can return a pointer to a real context of same order.
    #[inline(never)]
    unsafe fn create_successors(&mut self) -> Option<NonNull<Context>> {
        unsafe {
            let mut c = self.min_context;
            let mut up_branch = self.found_state.as_ref().successor_0 as u32
                | (self.found_state.as_ref().successor_1 as u32) << 16;
            let mut num_ps = 0;
            let mut ps: [Option<NonNull<State>>; 64] = [None; 64];

            if self.order_fall != 0 {
                let fresh = num_ps;
                num_ps += 1;
                ps[fresh as usize] = Some(self.found_state);
            }

            while c.as_ref().suffix != 0 {
                let mut s;
                c = self.ptr_of_offset(c.as_ref().suffix as isize).cast();

                if c.as_ref().num_stats != 1 {
                    let sym = self.found_state.as_ref().symbol;
                    s = self.get_stats(c);
                    while s.as_ref().symbol != sym {
                        s = s.offset(1);
                    }
                } else {
                    s = self.get_one_state(c);
                }
                let successor =
                    s.as_ref().successor_0 as u32 | (s.as_ref().successor_1 as u32) << 16;
                if successor != up_branch {
                    // c is the real record Context here.
                    c = self.ptr_of_offset(successor as isize).cast();
                    if num_ps == 0 {
                        // c is the real record MAX Order Context here,
                        // so we don't need to create any new contexts.
                        return Some(c);
                    }
                    break;
                } else {
                    let fresh2 = num_ps;
                    num_ps += 1;
                    ps[fresh2 as usize] = Some(s);
                }
            }

            // All created contexts will have single-symbol with new RAW-successor
            // All new RAW-successors will point to next position in RAW text
            // after `found_state.successor`
            let new_freq;
            let new_sym = *self.ptr_of_offset(up_branch as isize).cast::<u8>().as_ref();
            up_branch += 1;

            if c.as_ref().num_stats == 1 {
                new_freq = self.get_one_state(c).as_ref().freq;
            } else {
                let mut s = self.get_stats(c);
                while s.as_ref().symbol != new_sym {
                    s = s.offset(1);
                }
                let cf = (s.as_ref().freq as u32) - 1;
                let s0 = (c.as_ref().union2.summ_freq as u32) - c.as_ref().num_stats as u32 - cf;

                // cf - is frequency of symbol that will be successor in new context records.
                // s0 - is commutative frequency sum of another symbols from parent context.
                // max(new_freq) = (s.freq + 1), when (s0 == 1)
                // We have a requirement (Context::get_one_state().freq <= 128) in bin_summ
                // so (s.freq < 128) - is a requirement for multi-symbol contexts.
                new_freq = 1
                    + (if 2 * cf <= s0 {
                        (5 * cf > s0) as u32
                    } else {
                        ((2 * cf + s0 - 1) / (2 * s0)) + 1
                    }) as u8;
            }

            // Create a new single-symbol contexts from low order to high order in loop.
            loop {
                let mut c1 = if self.hi_unit != self.lo_unit {
                    self.hi_unit = self.hi_unit.offset(-UNIT_SIZE);
                    self.hi_unit.cast()
                } else if self.free_list[0] != 0 {
                    self.remove_node(0).cast()
                } else {
                    let c1 = self.alloc_units_rare(0)?;
                    c1.cast::<Context>()
                };

                c1.as_mut().num_stats = 1;
                {
                    let c1_state = self.get_one_state(c1).as_mut();
                    c1_state.symbol = new_sym;
                    c1_state.freq = new_freq;
                    Self::set_successor(c1_state, up_branch);
                }
                c1.as_mut().suffix = self.offset_for_ptr(c.cast());
                num_ps -= 1;
                let mut successor = ps[num_ps as usize].expect("successor not set");
                Self::set_successor(successor.as_mut(), self.offset_for_ptr(c1.cast()));
                c = c1;
                if num_ps == 0 {
                    break;
                }
            }

            Some(c)
        }
    }

    #[inline(never)]
    unsafe fn update_model(&mut self) {
        unsafe {
            let mut c: NonNull<Context>;

            let mc = self.min_context;

            if self.found_state.as_ref().freq < MAX_FREQ / 4 && mc.as_ref().suffix != 0 {
                // Update freqs in suffix context
                c = self.ptr_of_offset(mc.as_ref().suffix as isize).cast();

                if c.as_ref().num_stats == 1 {
                    let s = self.get_one_state(c).as_mut();
                    if s.freq < 32 {
                        s.freq += 1;
                    }
                } else {
                    let mut s = self.get_stats(c);
                    let sym = self.found_state.as_ref().symbol;
                    if s.as_ref().symbol != sym {
                        while s.as_ref().symbol != sym {
                            s = s.offset(1);
                        }
                        if s.offset(0).as_ref().freq >= s.offset(-1).as_ref().freq {
                            Self::swap_states(s);
                            s = s.offset(-1);
                        }
                    }
                    if s.as_ref().freq < MAX_FREQ - 9 {
                        s.as_mut().freq += 2;
                        c.as_mut().union2.summ_freq += 2;
                    }
                }
            }

            if self.order_fall == 0 {
                // MAX ORDER context
                // (found_state.Successor) is RAW-successor.
                match self.create_successors() {
                    None => {
                        self.restart_model();
                        return;
                    }
                    Some(mc) => {
                        self.min_context = mc;
                        self.max_context = mc;
                    }
                }

                Self::set_successor(
                    self.found_state.as_mut(),
                    self.offset_for_ptr(self.min_context.cast()),
                );
                return;
            }

            // NON MAX ORDER context
            let mut text = self.text;
            let mut fresh = text;
            text = text.offset(1);
            *fresh.as_mut() = self.found_state.as_ref().symbol;
            self.text = text;
            if text >= self.units_start {
                self.restart_model();
                return;
            }
            let mut max_successor = self.offset_for_ptr(text);

            let mut min_successor = self.found_state.as_ref().successor_0 as u32
                | (self.found_state.as_ref().successor_1 as u32) << 16;

            match min_successor {
                0 => {
                    // found_state has NULL-successor here.
                    // And only root 0-order context can contain NULL-successors.
                    // We change successor in found_state to RAW-successor,
                    // And next context will be same 0-order root Context.
                    Self::set_successor(self.found_state.as_mut(), max_successor);
                    min_successor = self.offset_for_ptr(self.min_context.cast());
                }
                _ => {
                    // There is a successor for found_state in min_context.
                    // So the next context will be one order higher than min_context.

                    if min_successor <= max_successor {
                        // min_successor is RAW-successor. So we will create real contexts records:
                        match self.create_successors() {
                            None => {
                                self.restart_model();
                                return;
                            }
                            Some(context) => {
                                min_successor = self.offset_for_ptr(context.cast());
                            }
                        }
                    }

                    // min_successor now is real Context pointer that points to existing (Order+1) context.

                    self.order_fall -= 1;
                    if self.order_fall == 0 {
                        // If we move to max_order context, then min_successor will be common Successor for both:
                        //   min_context that is (max_order - 1)
                        //   max_context that is (max_order)
                        // so we don't need new RAW-successor, and we can use real min_successor
                        // as successors for both min_context and max_context.
                        max_successor = min_successor;

                        // if (max_context != min_context)
                        // {
                        //   There was order fall from max_order, and we don't need current symbol
                        //   to transfer some RAW-successors to real contexts.
                        //   So we roll back pointer in raw data for one position.
                        // }
                        self.text = self
                            .text
                            .offset(-((self.max_context != self.min_context) as isize));
                    }
                }
            }

            let mc = self.min_context;
            c = self.max_context;

            self.min_context = self.ptr_of_offset(min_successor as isize).cast();
            self.max_context = self.min_context;

            if c == mc {
                return;
            }

            // s0 : is pure escape freq
            let ns = mc.as_ref().num_stats as u32;
            let s0 = (mc.as_ref().union2.summ_freq as u32)
                - ns
                - ((self.found_state.as_ref().freq as u32) - 1);

            while c != mc {
                let mut sum;
                let ns1 = c.as_ref().num_stats as u32;
                if ns1 != 1 {
                    if ns1 & 1 == 0 {
                        // Expand for one unit
                        let old_nu = ns1 >> 1;
                        let i = self.units2index[(old_nu as usize) - 1] as u32;
                        if i != self.units2index[old_nu as usize] as u32 {
                            let Some(ptr) = self.alloc_units(i + 1) else {
                                self.restart_model();
                                return;
                            };

                            let old_ptr = self.get_stats(c).cast();
                            Self::mem_12_copy(ptr, old_ptr, old_nu);

                            self.insert_node(old_ptr, i);
                            c.as_mut().union4.stats = self.offset_for_ptr(ptr);
                        }
                    }
                    sum = c.as_mut().union2.summ_freq as u32;
                    // Max increase of escape_freq is 3 here.
                    // Total increase of union2.summ_freq for all symbols is less than 256 here.
                    sum += ((2 * (ns1) < ns) as u32)
                        + 2 * ((4 * (ns1) <= ns) as u32 & (sum <= (8 * (ns1))) as u32);
                } else {
                    // Instead of 1-symbol context we create 2-symbol context.
                    let Some(s) = self.alloc_units(0) else {
                        self.restart_model();
                        return;
                    };
                    let mut s = s.cast::<State>();

                    let mut freq = c.as_ref().union2.state2.freq as u32;
                    s.as_mut().symbol = c.as_ref().union2.state2.symbol;
                    s.as_mut().successor_0 = c.as_ref().union4.state4.successor_0;
                    s.as_mut().successor_1 = c.as_ref().union4.state4.successor_1;
                    c.as_mut().union4.stats = self.offset_for_ptr(s.cast());
                    if freq < (MAX_FREQ / 4 - 1) as u32 {
                        freq <<= 1;
                    } else {
                        freq = (MAX_FREQ - 4) as u32;
                    }
                    // (max(s.freq) == 120), when we convert from 1-symbol into 2-symbol context.
                    s.as_mut().freq = freq as u8;
                    // max(init_esc = K_EXP_ESCAPE[*]) is 25. So the max(escape_freq) is 26 here.
                    sum = freq + self.init_esc + ((ns > 3) as u32);
                }

                let mut s = self.get_stats(c).offset(ns1 as isize);
                let mut cf = 2 * (sum + 6) * self.found_state.as_ref().freq as u32;
                let sf = s0 + sum;
                s.as_mut().symbol = self.found_state.as_ref().symbol;
                c.as_mut().num_stats = (ns1 + 1) as u16;

                Self::set_successor(s.as_mut(), max_successor);
                if cf < 6 * sf {
                    cf = 1 + ((cf > sf) as u32) + ((cf >= 4 * sf) as u32);
                    sum += 3;
                    // It can add (0, 1, 2) to escape_freq
                } else {
                    cf = 4
                        + ((cf >= 9 * sf) as u32)
                        + ((cf >= 12 * sf) as u32)
                        + ((cf >= 15 * sf) as u32);
                    sum += cf;
                }

                c.as_mut().union2.summ_freq = sum as u16;
                s.as_mut().freq = cf as u8;

                c = self.ptr_of_offset(c.as_ref().suffix as isize).cast();
            }
        }
    }

    unsafe fn swap_states(s: NonNull<State>) {
        unsafe {
            let tmp = *s.offset(0).as_ref();
            *s.offset(0).as_mut() = *s.offset(-1).as_ref();
            *s.offset(-1).as_mut() = tmp;
        }
    }

    unsafe fn mem_12_copy(ptr: NonNull<u8>, old_ptr: NonNull<u8>, old_nu: u32) {
        unsafe {
            let mut d = ptr.cast::<u32>();
            let mut z = old_ptr.cast::<u32>();
            for _ in 0..old_nu {
                *d.offset(0).as_mut() = *z.offset(0).as_ref();
                *d.offset(1).as_mut() = *z.offset(1).as_ref();
                *d.offset(2).as_mut() = *z.offset(2).as_ref();
                z = z.offset(3);
                d = d.offset(3);
            }
        }
    }

    #[inline(never)]
    unsafe fn rescale(&mut self) {
        unsafe {
            let stats = self.get_stats(self.min_context);
            let mut s = self.found_state;

            // Sort the list by freq
            if s != stats {
                let tmp = *s.as_ref();
                while s != stats {
                    *s.offset(0).as_mut() = *s.offset(-1).as_ref();
                    s = s.offset(-1);
                }
                *s.as_mut() = tmp;
            }

            let mut sum_freq = s.as_ref().freq as u32;
            let mut esc_freq = (self.min_context.as_ref().union2.summ_freq as u32) - sum_freq;

            // if (p.order_fall == 0), adder = 0 : it's     allowed to remove symbol from     MAX order context
            // if (p.order_fall != 0), adder = 1 : it's NOT allowed to remove symbol from NON-MAX order context

            let adder = (self.order_fall != 0) as u32;

            sum_freq = (sum_freq + 4 + adder) >> 1;
            let mut i = (self.min_context.as_ref().num_stats as u32) - 1;
            s.as_mut().freq = sum_freq as u8;

            for _ in 0..i {
                s = s.offset(1);
                let mut freq = s.as_ref().freq as u32;
                esc_freq -= freq;
                freq = (freq + adder) >> 1;
                sum_freq += freq;
                s.as_mut().freq = freq as u8;
                if freq > s.offset(-1).as_ref().freq as u32 {
                    let tmp = *s.as_mut();
                    let mut s1 = s;
                    loop {
                        *s1.offset(0).as_mut() = *s1.offset(-1).as_ref();
                        s1 = s1.offset(-1);
                        if !(s1 != stats && freq > s1.offset(-1).as_ref().freq as u32) {
                            break;
                        }
                    }
                    *s1.as_mut() = tmp;
                }
            }

            if s.as_ref().freq as i32 == 0 {
                // Remove all items with freq == 0

                i = 0;
                while s.as_ref().freq == 0 {
                    i += 1;
                    s = s.offset(-1);
                }

                // We increase (esc_freq) for the number of removed symbols.
                // So we will have (0.5) increase for escape_freq in average per
                // removed symbol after escape_freq halving
                esc_freq += i;
                let mut mc = self.min_context;
                let num_stats = mc.as_ref().num_stats as u32;
                let num_stats_new = num_stats.wrapping_sub(i);
                mc.as_mut().num_stats = num_stats_new as u16;
                let n0 = (num_stats + 1) >> 1;

                if num_stats_new == 1 {
                    // Create Single-Symbol context
                    let mut freq = stats.as_ref().freq as u32;
                    loop {
                        esc_freq >>= 1;
                        freq += 1 >> 1;
                        if esc_freq <= 1 {
                            break;
                        }
                    }

                    s = self.get_one_state(mc);
                    *s.as_mut() = *stats.as_ref();
                    s.as_mut().freq = freq as u8; // (freq <= 260 / 4)
                    self.found_state = s;
                    self.insert_node(stats.cast(), self.units2index[(n0 as usize) - 1] as u32);
                    return;
                }

                let n1 = (num_stats_new + 1) >> 1;
                if n0 != n1 {
                    let i0 = self.units2index[(n0 as usize) - 1] as u32;
                    let i1 = self.units2index[(n1 as usize) - 1] as u32;
                    if i0 != i1 {
                        if self.free_list[i1 as usize] != 0 {
                            let ptr = self.remove_node(i1);
                            self.min_context.as_mut().union4.stats = self.offset_for_ptr(ptr);
                            Self::mem_12_copy(ptr, stats.cast(), n1);
                            self.insert_node(stats.cast(), i0);
                        } else {
                            self.split_block(stats.cast(), i0, i1);
                        }
                    }
                }
            }

            // escape_freq halving here.
            self.min_context.as_mut().union2.summ_freq =
                (sum_freq + esc_freq - (esc_freq >> 1)) as u16;
            self.found_state = self.get_stats(self.min_context);
        }
    }

    unsafe fn make_esc_freq(&mut self, num_masked: u32, esc_freq: &mut u32) -> SeeSource {
        unsafe {
            let num_stats = self.min_context.as_ref().num_stats as u32;

            if num_stats != 256 {
                let non_masked = num_stats - num_masked;

                let (base_context_idx, see_table_hash) = self.calculate_see_table_hash(
                    self.min_context.as_ref(),
                    num_masked,
                    num_stats,
                    non_masked,
                );

                let see = &mut self.see[base_context_idx][see_table_hash];

                // If (see.summ) field is larger than 16-bit, we need only low 16 bits of summ.
                let summ = see.summ as u32;
                let r = summ >> see.shift as i32;
                see.summ = (summ - r) as u16;
                *esc_freq = r + (r == 0) as u32;

                SeeSource::Table(base_context_idx, see_table_hash)
            } else {
                *esc_freq = 1;
                SeeSource::Dummy
            }
        }
    }

    fn get_see(&mut self, see_source: SeeSource) -> &mut See {
        match see_source {
            SeeSource::Dummy => &mut self.dummy_see,
            SeeSource::Table(i, k) => &mut self.see[i][k],
        }
    }

    unsafe fn calculate_see_table_hash(
        &self,
        mc: &Context,
        num_masked: u32,
        num_stats: u32,
        non_masked: u32,
    ) -> (usize, usize) {
        unsafe {
            let base_context_idx = self.ns2index[(non_masked as usize) - 1] as usize;

            let suffix_context = self.ptr_of_offset(mc.suffix as isize).cast::<Context>();
            let suffix_num_stats = suffix_context.as_ref().num_stats as u32;
            let summ_freq = mc.union2.summ_freq as u32;

            let context_hierarchy_hash = (non_masked < (suffix_num_stats - num_stats)) as usize;
            let freq_distribution_hash = 2 * (summ_freq < (11 * num_stats)) as usize;
            let symbol_masking_ratio_hash = 4 * (num_masked > non_masked) as usize;
            let symbol_characteristics_hash = self.hi_bits_flag as usize;

            let see_table_hash = context_hierarchy_hash
                + freq_distribution_hash
                + symbol_masking_ratio_hash
                + symbol_characteristics_hash;

            (base_context_idx, see_table_hash)
        }
    }

    unsafe fn next_context(&mut self) {
        unsafe {
            let c = self.get_successor(self.found_state);
            if self.order_fall == 0 && c.addr() > self.text.addr() {
                self.min_context = c;
                self.max_context = self.min_context;
            } else {
                self.update_model();
            };
        }
    }

    unsafe fn update1(&mut self) {
        unsafe {
            let mut s = self.found_state;
            let freq = s.as_ref().freq as u32 + 4;
            self.min_context.as_mut().union2.summ_freq += 4;
            s.as_mut().freq = freq as u8;
            if freq > s.offset(-1).as_mut().freq as u32 {
                Self::swap_states(s);
                s = s.offset(-1);
                self.found_state = s;
                if freq > MAX_FREQ as u32 {
                    self.rescale();
                }
            }
            self.next_context();
        }
    }

    unsafe fn update1_0(&mut self) {
        unsafe {
            let s = self.found_state.as_mut();
            let mc = self.min_context.as_mut();
            let mut freq = s.freq as u32;
            let summ_freq = mc.union2.summ_freq as u32;
            self.prev_success = ((2 * freq) > summ_freq) as u32;
            self.run_length += self.prev_success as i32;
            mc.union2.summ_freq = (summ_freq + 4) as u16;
            freq += 4;
            s.freq = freq as u8;
            if freq > MAX_FREQ as u32 {
                self.rescale();
            }
            self.next_context();
        }
    }

    #[inline(always)]
    unsafe fn update_bin(&mut self, mut s: NonNull<State>) {
        unsafe {
            let freq = s.as_ref().freq as u32;
            self.found_state = s;
            self.prev_success = 1;
            self.run_length += 1;
            s.as_mut().freq += ((freq < 128) as u32) as u8;
            self.next_context();
        }
    }

    unsafe fn update2(&mut self) {
        unsafe {
            let s = self.found_state.as_mut();
            let freq = s.freq as u32 + 4;
            self.run_length = self.init_rl;
            self.min_context.as_mut().union2.summ_freq += 4;
            s.freq = freq as u8;
            if freq > MAX_FREQ as u32 {
                self.rescale();
            }
            self.update_model();
        }
    }

    #[inline(always)]
    unsafe fn mask_symbols(char_mask: &mut [u8; 256], s: NonNull<State>, mut s2: NonNull<State>) {
        unsafe {
            char_mask[s.as_ref().symbol as usize] = 0;
            while s2.addr() < s.addr() {
                let sym0 = s2.offset(0).as_ref().symbol as u32;
                let sym1 = s2.offset(1).as_ref().symbol as u32;
                s2 = s2.offset(2);
                char_mask[sym0 as usize] = 0;
                char_mask[sym1 as usize] = 0;
            }
        }
    }

    unsafe fn hi_bits_flag3(symbol: u32) -> u32 {
        (symbol + 0xC0) >> (8 - 3) & (1 << 3)
    }

    unsafe fn hi_bits_flag4(symbol: u32) -> u32 {
        (symbol + 0xC0) >> (8 - 4) & (1 << 4)
    }

    unsafe fn get_bin_summ(&mut self) -> &mut u16 {
        unsafe {
            let state = self.get_one_state(self.min_context);

            let hi_bits_flag3 = Self::hi_bits_flag3(self.found_state.as_ref().symbol as u32);
            let symbol = state.as_ref().symbol as u32;
            let hi_bits_flag4 = Self::hi_bits_flag4(symbol);

            self.hi_bits_flag = hi_bits_flag3;

            let freq_bin_idx = state.as_ref().freq as usize;

            let num_stats = self
                .ptr_of_offset(self.min_context.as_ref().suffix as isize)
                .cast::<Context>()
                .as_ref()
                .num_stats as usize;

            let context_idx = (self.prev_success
                + ((self.run_length as u32 >> 26) & 0x20)
                + (self.ns2bs_index[num_stats - 1] as u32)
                + hi_bits_flag4
                + hi_bits_flag3) as usize;

            &mut self.bin_summ[freq_bin_idx - 1][context_idx]
        }
    }

    #[inline(always)]
    unsafe fn get_context(&mut self, suffix: u32) -> NonNull<Context> {
        unsafe { self.ptr_of_offset(suffix as isize).cast() }
    }

    #[inline(always)]
    fn get_one_state(&mut self, context: NonNull<Context>) -> NonNull<State> {
        let context_ptr = context.as_ptr();
        // # Safety: Save because we got the pointer from a NonNull<State>.
        unsafe {
            let union2_ptr = std::ptr::addr_of_mut!((*context_ptr).union2);
            NonNull::new_unchecked(union2_ptr as *mut State).cast()
        }
    }

    #[inline(always)]
    unsafe fn get_stats(&mut self, mut context: NonNull<Context>) -> NonNull<State> {
        unsafe {
            self.ptr_of_offset(context.as_mut().union4.stats as isize)
                .cast()
        }
    }
}

impl<R: Read> Pppmd7<RangeDecoder<R>> {
    pub fn new_decoder(
        reader: R,
        order: u32,
        mem_size: u32,
    ) -> Result<Pppmd7<RangeDecoder<R>>, Error> {
        let range_decoder = RangeDecoder::new(reader)?;
        Self::new(range_decoder, order, mem_size)
    }

    pub fn range_decoder_code(&self) -> u32 {
        self.rc.code
    }
}

impl<W: Write> Pppmd7<RangeEncoder<W>> {
    pub fn new_encoder(
        writer: W,
        order: u32,
        mem_size: u32,
    ) -> Result<Pppmd7<RangeEncoder<W>>, Error> {
        let range_encoder = RangeEncoder::new(writer);
        Self::new(range_encoder, order, mem_size)
    }

    pub fn flush_range_encoder(&mut self) -> Result<(), std::io::Error> {
        self.rc.flush()
    }
}
