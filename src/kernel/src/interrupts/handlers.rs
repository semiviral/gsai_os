use crate::{
    interrupts::Vector,
    proc::{ElfData, Registers, State},
};
use libsys::{Address, Page, Virtual};

/// Indicates what type of error the common page fault handler encountered.
#[derive(Debug, Clone, Copy)]
pub struct PageFaultHandlerError;

/// ### Safety
///
/// This function should only be called in the case of passing context to handle a page fault.
/// Calling this function more than once and/or outside the context of a page fault is undefined behaviour.
#[doc(hidden)]
#[repr(align(0x10))]
pub unsafe fn pf_handler(address: Address<Virtual>) -> Result<(), PageFaultHandlerError> {
    crate::local::with_scheduler(|scheduler| {
        use crate::memory::paging::TableEntryFlags;

        let process = scheduler.process_mut().ok_or(PageFaultHandlerError)?;
        let elf_vaddr = process
            .load_address_to_elf_vaddr(address)
            .unwrap_or_else(|| panic!("failed to calculate ELF address for page fault: {:X?}", address));
        let phdr = process
            .elf_segments()
            .iter()
            .filter(|phdr| phdr.p_type == elf::abi::PT_LOAD)
            .find(|phdr| (phdr.p_vaddr..(phdr.p_vaddr + phdr.p_memsz)).contains(&u64::try_from(elf_vaddr).unwrap()))
            .ok_or(PageFaultHandlerError)?
            .clone();

        // Small check to help ensure the phdr alignments are page-fit.
        debug_assert_eq!(phdr.p_align & (libsys::page_mask() as u64), 0);
        trace!("Demand mapping from segment: {:?}", phdr);

        // Map the page as RW so we can copy the ELF data in.
        let mapped_memory = process
            .address_space_mut()
            .mmap(
                Some(Address::<Page>::new_truncate(address.get())),
                core::num::NonZeroUsize::MIN,
                crate::proc::MmapPermissions::ReadWrite,
            )
            .unwrap();

        // Calculate the range of bytes we will be reading from the ELF file.

        let segment_vaddr = usize::try_from(phdr.p_vaddr).unwrap();
        // This calculation represents the byte offset of the faulting address from the segment start.
        let segment_offset = elf_vaddr - segment_vaddr;
        // Using the byte offset we just calculated, we can apply that offset to the
        //  beginning of the segment to get the ELF's file memory.
        let file_start = usize::try_from(phdr.p_offset).unwrap() + segment_offset;
        // Then, with the file size, we calculate the end of the absolute file range.
        let file_end = file_start + usize::try_from(phdr.p_filesz).unwrap();
        let file_range = file_start..file_end;

        // Subslice the ELF memory to get the requisite segment data.
        let file_slice = match process.elf_data() {
            ElfData::Memory(elf_memory) => &elf_memory[file_range],
            ElfData::File(_) => unimplemented!(),
        };

        // Load the ELF data.
        let mapped_memory = mapped_memory.as_uninit_slice_mut();
        // Front padding is all of the bytes before the file offset.
        let (front_pad, remaining) = mapped_memory.split_at_mut(file_start % mapped_memory.len());
        // Clamp the file slice length to ensure we don't try to split out-of-bounds.
        let file_memory_split_index = usize::min(file_slice.len(), remaining.len());
        // End padding is all of the bytes after the file offset + file slice length.
        let (file_memory, end_pad) = remaining.split_at_mut(file_memory_split_index);
        // Zero the padding bytes, according to ELF spec.
        front_pad.fill(core::mem::MaybeUninit::new(0x0));
        end_pad.fill(core::mem::MaybeUninit::new(0x0));
        // Safety: In-place cast to a transparently aligned type.
        // Clamping the file slice ensures we don't copy out-of-bounds.
        let file_slice_clamped = &file_slice[..file_memory.len()];
        // Copy the ELF data into memory.
        file_memory.copy_from_slice(unsafe { file_slice_clamped.align_to().1 });

        // Process any relocations.
        let load_offset = process.load_offset();
        let phdr_mem_range = phdr.p_vaddr..(phdr.p_vaddr + phdr.p_memsz);
        process.elf_relas().drain_filter(|rela| {
            if phdr_mem_range.contains(&u64::try_from(rela.address.get()).unwrap()) {
                info!("Processing relocation: {:X?}", rela);
                rela.address.as_ptr().add(load_offset).cast::<usize>().write(rela.value);
                true
            } else {
                false
            }
        });

        process
            .address_space_mut()
            .set_flags(
                Address::<Page>::new_truncate(address.get()),
                core::num::NonZeroUsize::MIN,
                TableEntryFlags::PRESENT
                    | TableEntryFlags::USER
                    | TableEntryFlags::from(crate::proc::segment_type_to_mmap_permissions(phdr.p_type)),
            )
            .unwrap();

        Ok(())
    })
}

/// ### Safety
///
/// This function should only be called in the case of passing context to handle an interrupt.
/// Calling this function more than once and/or outside the context of an interrupt is undefined behaviour.
#[doc(hidden)]
#[repr(align(0x10))]
pub unsafe fn handle_irq(irq_vector: u64, state: &mut State, regs: &mut Registers) {
    match Vector::try_from(irq_vector) {
        Ok(Vector::Timer) => crate::local::with_scheduler(|scheduler| scheduler.next_task(state, regs)),

        Err(err) => panic!("Invalid interrupt vector: {:X?}", err),
        vector_result => unimplemented!("Unhandled interrupt: {:?}", vector_result),
    }

    #[cfg(target_arch = "x86_64")]
    crate::local::end_of_interrupt();
}
