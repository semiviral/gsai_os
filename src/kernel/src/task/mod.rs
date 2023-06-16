mod context;

use bit_field::BitField;
pub use context::*;

mod scheduling;
pub use scheduling::*;

mod address_space;
pub use address_space::*;

use crate::mem::alloc::AlignedAllocator;
use alloc::{boxed::Box, string::String, vec::Vec};
use core::num::NonZeroUsize;
use elf::{endian::AnyEndian, file::FileHeader, segment::ProgramHeader};
use libsys::{page_size, Address, Virtual};

#[allow(clippy::cast_possible_truncation)]
pub const STACK_SIZE: NonZeroUsize = NonZeroUsize::new((libsys::MIBIBYTE as usize) - page_size()).unwrap();
pub const STACK_PAGES: NonZeroUsize = NonZeroUsize::new(STACK_SIZE.get() / page_size()).unwrap();
pub const STACK_START: NonZeroUsize = NonZeroUsize::new(page_size()).unwrap();
pub const MIN_LOAD_OFFSET: usize = STACK_START.get() + STACK_SIZE.get();

pub const PT_FLAG_EXEC_BIT: usize = 0;
pub const PT_FLAG_WRITE_BIT: usize = 1;

pub fn segment_to_mmap_permissions(segment_ty: u32) -> MmapPermissions {
    match (segment_ty.get_bit(PT_FLAG_WRITE_BIT), segment_ty.get_bit(PT_FLAG_EXEC_BIT)) {
        (true, false) => MmapPermissions::ReadWrite,
        (false, true) => MmapPermissions::ReadExecute,
        (false, false) => MmapPermissions::ReadOnly,
        (true, true) => panic!("ELF section is WX"),
    }
}

pub static TASK_LOAD_BASE: usize = 0x20000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Idle = 0,
    Low = 1,
    Normal = 2,
    High = 3,
    Critical = 4,
}

#[derive(Debug, Clone, Copy)]
pub struct ElfRela {
    pub address: Address<Virtual>,
    pub value: usize,
}

pub type Context = (State, Registers);
pub type ElfAllocator = AlignedAllocator<{ libsys::page_size() }>;
pub type ElfMemory = Box<[u8], ElfAllocator>;

#[derive(Debug)]
pub enum ElfData {
    Memory(ElfMemory),
    File(String),
}

pub struct Task {
    id: uuid::Uuid,
    priority: Priority,

    address_space: AddressSpace,
    context: Context,
    load_offset: usize,

    elf_header: FileHeader<AnyEndian>,
    elf_segments: Box<[ProgramHeader]>,
    elf_relas: Vec<ElfRela>,
    elf_data: ElfData,
}

impl Task {
    pub fn new(
        priority: Priority,
        mut address_space: AddressSpace,
        load_offset: usize,
        elf_header: FileHeader<AnyEndian>,
        elf_segments: Box<[ProgramHeader]>,
        elf_relas: Vec<ElfRela>,
        elf_data: ElfData,
    ) -> Self {
        let stack = address_space
            .mmap(Some(Address::new_truncate(STACK_START.get())), STACK_PAGES, MmapPermissions::ReadWrite)
            .unwrap();

        Self {
            id: uuid::Uuid::new_v4(),
            priority,
            address_space,
            context: (
                State::user(u64::try_from(load_offset).unwrap() + elf_header.e_entry, unsafe {
                    stack.as_non_null_ptr().as_ptr().add(stack.len()).addr() as u64
                }),
                Registers::default(),
            ),
            load_offset,
            elf_header,
            elf_segments,
            elf_relas,
            elf_data,
        }
    }

    #[inline]
    pub const fn id(&self) -> uuid::Uuid {
        self.id
    }

    #[inline]
    pub const fn priority(&self) -> Priority {
        self.priority
    }

    #[inline]
    pub const fn address_space(&self) -> &AddressSpace {
        &self.address_space
    }

    #[inline]
    pub fn address_space_mut(&mut self) -> &mut AddressSpace {
        &mut self.address_space
    }

    #[inline]
    pub const fn load_offset(&self) -> usize {
        self.load_offset
    }

    #[inline]
    pub const fn elf_header(&self) -> &FileHeader<AnyEndian> {
        &self.elf_header
    }

    #[inline]
    pub const fn elf_segments(&self) -> &[ProgramHeader] {
        &self.elf_segments
    }

    #[inline]
    pub const fn elf_data(&self) -> &ElfData {
        &self.elf_data
    }

    #[inline]
    pub fn elf_relas(&mut self) -> &mut Vec<ElfRela> {
        &mut self.elf_relas
    }
}

impl core::fmt::Debug for Task {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Task")
            .field("ID", &self.id)
            .field("Priority", &self.priority)
            .field("Address Space", &self.address_space)
            .field("Context", &self.context)
            .field("ELF Load Offset", &self.load_offset)
            .field("ELF Header", &self.elf_header)
            .finish_non_exhaustive()
    }
}
