use alloc::{alloc::Global, vec::Vec};
use bit_field::BitField;
use core::{alloc::AllocError, num::NonZeroUsize, sync::atomic::Ordering};
use libcommon::{
    memory::{page_aligned_allocator, AlignedAllocator, KernelAllocator},
    Address, Virtual,
};
use spin::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Unusable = 0,
    Generic = 1,
    Reserved = 2,
    BootReclaim = 3,
    AcpiReclaim = 4,
}

impl FrameType {
    fn from_u16(value: u16) -> Self {
        match value {
            0 => Self::Unusable,
            1 => Self::Generic,
            2 => Self::Reserved,
            3 => Self::BootReclaim,
            4 => Self::AcpiReclaim,
            _ => unimplemented!(),
        }
    }

    fn as_u16(self) -> u16 {
        match self {
            FrameType::Unusable => 0,
            FrameType::Generic => 1,
            FrameType::Reserved => 2,
            FrameType::BootReclaim => 3,
            FrameType::AcpiReclaim => 4,
        }
    }
}

#[derive(Debug)]
#[repr(transparent)]
pub struct Frame(core::sync::atomic::AtomicU16);

impl Frame {
    const PEEKED_SHIFT: usize = 0;
    const LOCKED_SHIFT: usize = 1;
    const TYPE_SHIFT: usize = 12;
    const PEEKED_BIT: u16 = 1 << Self::PEEKED_SHIFT;
    const LOCKED_BIT: u16 = 1 << Self::LOCKED_SHIFT;

    fn lock(&self) {
        let old_value = self.0.fetch_or(Self::LOCKED_BIT, Ordering::Relaxed);
        debug_assert!(!old_value.get_bit(Self::LOCKED_SHIFT));
    }

    fn free(&self) {
        let old_value = self.0.fetch_and(Self::LOCKED_BIT, Ordering::Relaxed);
        debug_assert!(old_value.get_bit(Self::LOCKED_SHIFT));
    }

    #[inline]
    fn try_peek(&self) -> bool {
        !self.0.fetch_or(Self::PEEKED_BIT, Ordering::Relaxed).get_bit(Self::PEEKED_SHIFT)
    }

    #[inline]
    fn peek(&self) {
        while !self.try_peek() {
            core::hint::spin_loop();
        }

        debug_assert!(self.0.load(Ordering::Relaxed).get_bit(Self::PEEKED_SHIFT));
    }

    #[inline]
    fn unpeek(&self) {
        debug_assert!(self.0.load(Ordering::Relaxed).get_bit(Self::PEEKED_SHIFT));

        self.0.fetch_and(!Self::PEEKED_BIT, Ordering::Relaxed);
    }

    /// Returns the frame data in a tuple.
    fn data(&self) -> (bool, FrameType) {
        debug_assert!(self.0.load(Ordering::Relaxed).get_bit(Self::PEEKED_SHIFT));

        let raw = self.0.load(Ordering::Relaxed);
        (raw.get_bit(Self::LOCKED_SHIFT), FrameType::from_u16(raw >> Self::TYPE_SHIFT))
    }

    fn modify_type(&self, new_type: FrameType) {
        debug_assert!(self.0.load(Ordering::Relaxed).get_bit(Self::PEEKED_SHIFT));

        self.0
            .store(*self.0.load(Ordering::Relaxed).set_bits(Self::TYPE_SHIFT.., new_type.as_u16()), Ordering::Relaxed);
    }
}

type Result<T> = core::result::Result<T, AllocError>;

pub struct SlabAllocator<'a> {
    slabs64: Mutex<Vec<(*mut u8, u64), AlignedAllocator<0x1000, Global>>>,
    slabs128: Mutex<Vec<(*mut u8, u32), AlignedAllocator<0x1000, Global>>>,
    slabs256: Mutex<Vec<(*mut u8, u16), AlignedAllocator<0x1000, Global>>>,
    slabs512: Mutex<Vec<(*mut u8, u8), AlignedAllocator<0x1000, Global>>>,
    phys_mapped_address: Address<Virtual>,
    table: &'a [Frame],
}

// SAFETY: Type uses a global physical mapped address, and so is thread-independent.
unsafe impl Send for SlabAllocator<'_> {}
// SAFETY: Type ensures all concurrent accesses are synchronized.
unsafe impl Sync for SlabAllocator<'_> {}

impl<'a> SlabAllocator<'a> {
    pub fn from_memory_map(memory_map: &[crate::MmapEntry], phys_mapped_address: Address<Virtual>) -> Option<Self> {
        let page_count = libcommon::align_up_div(
            memory_map.last().map(|entry| entry.base + entry.len).unwrap() as usize,
            // SAFETY: Value provided is non-zero.
            unsafe { NonZeroUsize::new_unchecked(0x1000) },
        );
        let table_bytes = libcommon::align_up(
            page_count * core::mem::size_of::<Frame>(),
            // SAFETY: Value provided is non-zero.
            unsafe { NonZeroUsize::new_unchecked(0x1000) },
        );

        let table_entry = memory_map
            .iter()
            .filter(|entry| entry.typ == limine::LimineMemoryMapEntryType::Usable)
            .find(|entry| entry.len >= (table_bytes as u64))?;

        // Clear the memory of the chosen region.
        unsafe { core::ptr::write_bytes(table_entry.base as *mut u8, 0, table_bytes) };

        let table = unsafe {
            core::slice::from_raw_parts((phys_mapped_address.as_u64() + table_entry.base) as *mut Frame, page_count)
        };

        for entry in memory_map {
            assert_eq!(entry.base & 0xFFF, 0, "memory map entry is not page-aligned: {entry:?}");

            let base_index = entry.base / 0x1000;
            let count = entry.len / 0x1000;

            // Translate UEFI memory type to kernel frame type.
            let frame_ty = {
                use crate::MmapEntryType;

                match entry.typ {
                    MmapEntryType::Usable => FrameType::Generic,
                    MmapEntryType::BootloaderReclaimable => FrameType::BootReclaim,
                    MmapEntryType::AcpiReclaimable => FrameType::AcpiReclaim,
                    MmapEntryType::KernelAndModules
                    | MmapEntryType::Reserved
                    | MmapEntryType::AcpiNvs
                    | MmapEntryType::Framebuffer => FrameType::Reserved,
                    MmapEntryType::BadMemory => FrameType::Unusable,
                }
            };

            (base_index..(base_index + count)).map(|index| &table[index as usize]).for_each(|frame| {
                frame.peek();
                frame.modify_type(frame_ty);
                frame.unpeek();
            });
        }

        // Ensure the table pages are reserved, so as to not be locked by any of the `_next` functions.
        table
            .iter()
            .skip((table_entry.base / 0x1000) as usize)
            .take(libcommon::align_up_div(
                table_bytes,
                // SAFETY: Value provided is non-zero.
                unsafe { NonZeroUsize::new_unchecked(0x1000) },
            ))
            .for_each(|frame| {
                frame.peek();
                frame.modify_type(FrameType::Reserved);
                frame.unpeek();
            });

        Some(Self {
            slabs64: Mutex::new(Vec::new_in(page_aligned_allocator())),
            slabs128: Mutex::new(Vec::new_in(page_aligned_allocator())),
            slabs256: Mutex::new(Vec::new_in(page_aligned_allocator())),
            slabs512: Mutex::new(Vec::new_in(page_aligned_allocator())),
            phys_mapped_address,
            table,
        })
    }

    fn with_table<T>(&self, func: impl FnOnce(&[Frame]) -> T) -> T {
        libarch::interrupts::without(|| func(self.table))
    }
}

macro_rules! slab_allocate {
    ($self:expr, $slabs_name:ident, $slab_size:expr) => {
        {
            let mut slabs = $self.$slabs_name.lock();
            let allocation_ptr =
                match slabs.iter_mut().find(|(_, allocations)| allocations.trailing_zeros() > 0) {
                    Some((memory_ptr, allocations)) => {
                        let allocation_bit = (allocations.trailing_zeros() - 1) as usize;
                        allocations.set_bit(allocation_bit, true);

                        // SAFETY: Arbitrary `u8` memory region is valid for the offsets within its bounds.
                        unsafe { memory_ptr.add(allocation_bit * $slab_size) }
                    }

                    None if let Ok(frame) = $self.lock_next() => {
                        // SAFETY: `phys_mapped_address` is required to be valid for arbitrary offsets from within its range.
                        let memory_ptr = unsafe { $self.phys_mapped_address.as_mut_ptr::<u8>().add(frame.as_usize()) };
                        slabs.push((memory_ptr, 1 << ((0x1000 / $slab_size) - 1)));

                        memory_ptr
                    }

                    None => unimplemented!()
                };

            // SAFETY: This code is only reached once `allocation_ptr` is no longer `None`.
            Ok(slice_from_raw_parts_mut(allocation_ptr, $slab_size))
        }
    };
}

macro_rules! slab_deallocate {
    ($self:expr, $slabs_name:ident, $slab_size:expr, $ptr:expr) => {
        let ptr_addr = $ptr.addr().get();
        let mut slabs = $self.$slabs_name.lock();

        for (memory_ptr, allocations) in slabs.iter_mut() {
            let memory_range = memory_ptr.addr()..(memory_ptr.addr() + 4096);
            if memory_range.contains(&ptr_addr) {
                let allocation_offset = ptr_addr - memory_range.start;
                let allocation_bit = allocation_offset / $slab_size;
                allocations.set_bit(allocation_bit, false);
            }
        }
    };
}

// SAFETY: `SlabAllocator` promises to do everything right.
unsafe impl<'a> core::alloc::Allocator for SlabAllocator<'a> {
    fn allocate(&self, layout: core::alloc::Layout) -> Result<core::ptr::NonNull<[u8]>> {
        use core::ptr::{slice_from_raw_parts_mut, NonNull};

        let allocation_ptr = {
            if layout.align() <= 64 && layout.size() <= 64 {
                slab_allocate!(self, slabs64, 64)
            } else if layout.align() <= 128 && layout.size() <= 128 {
                slab_allocate!(self, slabs128, 128)
            } else if layout.align() <= 256 && layout.size() <= 246 {
                slab_allocate!(self, slabs256, 256)
            } else if layout.align() <= 512 && layout.size() <= 512 {
                slab_allocate!(self, slabs512, 512)
            } else {
                (if layout.align() <= 4096 && layout.size() <= 4096 {
                    self.lock_next()
                } else {
                    self.lock_next_many(
                        NonZeroUsize::new(layout.size() / 0x1000).unwrap(),
                        // SAFETY: `Layout::align()` can not be zero in safe Rust.
                        unsafe { NonZeroUsize::new_unchecked(layout.align()) },
                    )
                })
                .map(|address| {
                    // SAFETY: Frame addresses are naturally aligned, and arbitrary memory is valid for `u8`, and `phys_mapped_address` is
                    //         required to be valid for arbitrary offsets from within its range.
                    let allocation_ptr = unsafe { self.phys_mapped_address.as_mut_ptr::<u8>().add(address.as_usize()) };
                    slice_from_raw_parts_mut(
                        allocation_ptr,
                        libcommon::align_up(
                            layout.size(),
                            // SAFETY: Value provided is non-zero.
                            unsafe { NonZeroUsize::new_unchecked(0x1000) },
                        ),
                    )
                })
            }
        };

        match allocation_ptr {
            Ok(allocation_slice) if let Some(non_null) = NonNull::new(allocation_slice) => Ok(non_null),
            _ => Err(AllocError),
        }
    }

    unsafe fn deallocate(&self, ptr: core::ptr::NonNull<u8>, layout: core::alloc::Layout) {
        if layout.align() <= 64 && layout.size() <= 64 {
            slab_deallocate!(self, slabs64, 64, ptr);
        } else if layout.align() <= 128 && layout.size() <= 128 {
            slab_deallocate!(self, slabs128, 128, ptr);
        } else if layout.align() <= 256 && layout.size() <= 246 {
            slab_deallocate!(self, slabs256, 256, ptr);
        } else if layout.align() <= 512 && layout.size() <= 512 {
            slab_deallocate!(self, slabs512, 512, ptr);
        } else {
            todo!("don't leak memory")
        }
    }
}

impl KernelAllocator for SlabAllocator<'_> {
    fn lock_next(&self) -> Result<Address<libcommon::Frame>> {
        self.with_table(|table| {
            table
                .iter()
                .enumerate()
                .find_map(|(index, table_page)| {
                    if table_page.try_peek()
                        && let (locked, ty) = table_page.data()
                        && !locked && ty == FrameType::Generic {
                            table_page.lock();
                            table_page.unpeek();

                            Some(Address::<libcommon::Frame>::new_truncate((index * 0x1000) as u64))
                    } else {
                        table_page.unpeek();

                        None
                    }
                })
                .ok_or(AllocError)
        })
    }

    fn lock_next_many(&self, count: NonZeroUsize, alignment: NonZeroUsize) -> Result<Address<libcommon::Frame>> {
        debug_assert!(alignment.is_power_of_two());

        self.with_table(|table| {
            let frame_alignment = NonZeroUsize::new(alignment.get() / 0x1000).unwrap_or(
                // SAFETY: Value provided is known non-zero.
                unsafe { NonZeroUsize::new_unchecked(1) },
            );
            let mut start_index = 0;

            while start_index < (table.len() - count.get()) {
                let sub_table = &table[start_index..(start_index + count.get())];
                sub_table.iter().for_each(Frame::peek);

                match sub_table
                    .iter()
                    .enumerate()
                    .map(|(index, frame)| (index, frame.data()))
                    .rev() // Reverse the iterator to ensure we get the very last index that isn't viable (speeds up iterations).
                    .find(|(_, (locked, ty))| *locked || *ty != FrameType::Generic)
                {
                    Some((index, _)) => {
                        start_index = libcommon::align_up(index + 1, frame_alignment);
                        sub_table.iter().for_each(Frame::unpeek);
                    }
                    None => {
                        sub_table.iter().for_each(|frame| {
                            frame.lock();
                            frame.unpeek();
                        });

                        return Ok(Address::<libcommon::Frame>::new_truncate((start_index * 0x1000) as u64));
                    }
                }
            }

            Err(AllocError)
        })
    }

    fn lock(&self, frame: Address<libcommon::Frame>) -> Result<()> {
        self.with_table(|table| {
            let Some(frame) = table.get(frame.index()) else { return Err(AllocError) };
            frame.peek();

            let (locked, _) = frame.data();
            if !locked {
                frame.lock();
                frame.unpeek();

                Ok(())
            } else {
                frame.unpeek();

                Err(AllocError)
            }
        })
    }

    fn lock_many(&self, base: Address<libcommon::Frame>, count: usize) -> Result<()> {
        self.with_table(|table| {
            let frames = &table[base.index()..(base.index() + count)];
            frames.iter().for_each(Frame::peek);

            if frames.iter().map(Frame::data).all(|(locked, _)| !locked) {
                frames.iter().for_each(Frame::lock);
                frames.iter().for_each(Frame::unpeek);

                Ok(())
            } else {
                frames.iter().for_each(Frame::unpeek);

                Err(AllocError)
            }
        })
    }

    fn free(&self, frame: Address<libcommon::Frame>) -> Result<()> {
        self.with_table(|table| {
            let Some(frame) = table.get(frame.index()) else { return Err(AllocError) };

            frame.peek();

            match frame.data() {
                (locked, _) if locked => {
                    frame.free();
                    frame.unpeek();

                    Ok(())
                }

                _ => {
                    frame.unpeek();

                    Err(AllocError)
                }
            }
        })
    }

    // TODO non-zero usize for the count
    fn allocate_to(&self, frame: Address<libcommon::Frame>, count: usize) -> Result<Address<Virtual>> {
        self.lock_many(frame, count)
            .map(|_| Address::<Virtual>::new_truncate(self.phys_mapped_address.as_u64() + frame.as_u64()))
    }

    fn total_memory(&self) -> usize {
        self.table.len() * 0x1000
    }
}
