// Copyright (c) 2017 Colin Finck, RWTH Aachen University
//
// MIT License
//
// Permission is hereby granted, free of charge, to any person obtaining
// a copy of this software and associated documentation files (the
// "Software"), to deal in the Software without restriction, including
// without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to
// permit persons to whom the Software is furnished to do so, subject to
// the following conditions:
//
// The above copyright notice and this permission notice shall be
// included in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE
// LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION
// WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

include!(concat!(env!("CARGO_TARGET_DIR"), "/config.rs"));
include!(concat!(env!("CARGO_TARGET_DIR"), "/smp_boot_code.rs"));

use alloc::boxed::Box;
use alloc::vec::Vec;
use arch;
use arch::x86_64::kernel::acpi;
use arch::x86_64::kernel::idt;
use arch::x86_64::kernel::irq;
use arch::x86_64::mm::paging;
use arch::x86_64::mm::paging::{BasePageSize, PageSize, PageTableEntryFlags};
use arch::x86_64::mm::virtualmem;
use arch::x86_64::kernel::percore::*;
use arch::x86_64::kernel::processor;
use arch::x86_64::kernel::KERNEL_HEADER;
use core::sync::atomic::spin_loop_hint;
use core::{cmp, fmt, mem, ptr, str, u32};
use environment;
use mm;
use scheduler;
use x86::controlregs::*;
use x86::msr::*;

const APIC_ICR2: usize = 0x0310;

const APIC_DIV_CONF_DIVIDE_BY_8: u64        = 0b0010;
const APIC_EOI_ACK: u64                     = 0;
const APIC_ICR_DELIVERY_MODE_FIXED: u64     = 0x000;
const APIC_ICR_DELIVERY_MODE_INIT: u64      = 0x500;
const APIC_ICR_DELIVERY_MODE_STARTUP: u64   = 0x600;
const APIC_ICR_DELIVERY_STATUS_PENDING: u32 = 1 << 12;
const APIC_ICR_LEVEL_TRIGGERED: u64         = 1 << 15;
const APIC_ICR_LEVEL_ASSERT: u64            = 1 << 14;
const APIC_LVT_MASK: u64                    = 1 << 16;
const APIC_LVT_TIMER_TSC_DEADLINE: u64      = 1 << 18;
const APIC_SIVR_ENABLED: u64                = 1 << 8;

/// Register index: ID
#[allow(dead_code)]
const IOAPIC_REG_ID: u32					= 0x0000;
/// Register index: version
const IOAPIC_REG_VER: u32					= 0x0001;
/// Redirection table base
const IOAPIC_REG_TABLE: u32					= 0x0010;

const TLB_FLUSH_INTERRUPT_NUMBER: u8 = 112;
const WAKEUP_INTERRUPT_NUMBER: u8    = 121;
pub const TIMER_INTERRUPT_NUMBER: u8 = 123;
const ERROR_INTERRUPT_NUMBER: u8     = 126;
const SPURIOUS_INTERRUPT_NUMBER: u8  = 127;

/// Physical and virtual memory address for our SMP boot code.
///
/// While our boot processor is already in x86-64 mode, application processors boot up in 16-bit real mode
/// and need an address in the CS:IP addressing scheme to jump to.
/// The CS:IP addressing scheme is limited to 2^20 bytes (= 1 MiB).
const SMP_BOOT_CODE_ADDRESS: usize = 0x8000;

const SMP_BOOT_CODE_OFFSET_PML4: usize = 0x10;
const SMP_BOOT_CODE_OFFSET_ENTRY: usize = 0x08;

const X2APIC_ENABLE: u64 = 1 << 10;

static mut LOCAL_APIC_ADDRESS: usize = 0;
static mut IOAPIC_ADDRESS: usize = 0;

/// Stores the Local APIC IDs of all CPUs. The index equals the Core ID.
/// Both numbers often match, but don't need to (e.g. when a core has been disabled).
///
/// As Rust currently implements no way of zero-initializing a global Vec in a no_std environment,
/// we have to encapsulate it in an Option...
static mut CPU_LOCAL_APIC_IDS: Option<Vec<u8>> = None;

/// After calibration, initialize the APIC Timer with this counter value to let it fire an interrupt
/// after 1 microsecond.
static mut CALIBRATED_COUNTER_VALUE: u64 = 0;


#[repr(C, packed)]
struct AcpiMadtHeader {
	local_apic_address: u32,
	flags: u32,
}

#[repr(C, packed)]
struct AcpiMadtRecordHeader {
	entry_type: u8,
	length: u8,
}

#[repr(C, packed)]
struct ProcessorLocalApicRecord {
	acpi_processor_id: u8,
	apic_id: u8,
	flags: u32,
}

impl fmt::Display for ProcessorLocalApicRecord {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "{{ acpi_processor_id: {}, ", {self.acpi_processor_id})?;
		write!(f, "apic_id: {}, ", {self.apic_id})?;
		write!(f, "flags: {} }}", {self.flags})?;
		Ok(())
	}
}

const CPU_FLAG_ENABLED: u32 = 1 << 0;

#[repr(C, packed)]
struct IoApicRecord {
	id: u8,
	reserved: u8,
	address: u32,
	global_system_interrupt_base: u32,
}

impl fmt::Display for IoApicRecord {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "{{ id: {}, ", {self.id})?;
		write!(f, "reserved: {}, ", {self.reserved})?;
		write!(f, "address: {:#X}, ", {self.address})?;
		write!(f, "global_system_interrupt_base: {} }}", {self.global_system_interrupt_base})?;
		Ok(())
	}
}


extern "x86-interrupt" fn tlb_flush_handler(_stack_frame: &mut irq::ExceptionStackFrame) {
	debug!("Received TLB Flush Interrupt");
	unsafe { cr3_write(cr3()); }
	eoi();
}

extern "x86-interrupt" fn error_interrupt_handler(stack_frame: &mut irq::ExceptionStackFrame) {
	error!("APIC LVT Error Interrupt");
	error!("ESR: {:#X}", local_apic_read(IA32_X2APIC_ESR));
	error!("{:#?}", stack_frame);
	eoi();
	scheduler::abort();
}

extern "x86-interrupt" fn spurious_interrupt_handler(stack_frame: &mut irq::ExceptionStackFrame) {
	error!("Spurious Interrupt: {:#?}", stack_frame);
	scheduler::abort();
}

extern "x86-interrupt" fn wakeup_handler(_stack_frame: &mut irq::ExceptionStackFrame) {
	debug!("Received Wakeup Interrupt");
	eoi();
}


#[inline]
pub fn add_local_apic_id(id: u8) {
	unsafe { CPU_LOCAL_APIC_IDS.as_mut().unwrap().push(id); }
}

fn detect_from_acpi() -> Result<usize, ()> {
	// Get the Multiple APIC Description Table (MADT) from the ACPI information and its specific table header.
	let madt = acpi::get_madt().expect("HermitCore requires a MADT in the ACPI tables");
	let madt_header = unsafe { & *(madt.table_start_address() as *const AcpiMadtHeader) };

	// Jump to the actual table entries (after the table header).
	let mut current_address = madt.table_start_address() + mem::size_of::<AcpiMadtHeader>();

	// Loop through all table entries.
	while current_address < madt.table_end_address() {
		let record = unsafe { & *(current_address as *const AcpiMadtRecordHeader) };
		current_address += mem::size_of::<AcpiMadtRecordHeader>();

		match record.entry_type {
			0 => {
				// Processor Local APIC
				let processor_local_apic_record = unsafe { & *(current_address as *const ProcessorLocalApicRecord) };
				debug!("Found Processor Local APIC record: {}", processor_local_apic_record);

				if processor_local_apic_record.flags & CPU_FLAG_ENABLED > 0 {
					add_local_apic_id(processor_local_apic_record.apic_id);
				}
			},
			1 => {
				// I/O APIC
				let ioapic_record = unsafe { & *(current_address as *const IoApicRecord) };
				debug!("Found I/O APIC record: {}", ioapic_record);

				unsafe {
					IOAPIC_ADDRESS = virtualmem::allocate(BasePageSize::SIZE);
					debug!("Mapping IOAPIC at {:#X} to virtual address {:#X}", ioapic_record.address, IOAPIC_ADDRESS);

					let mut flags = PageTableEntryFlags::empty();
					flags.device().writable().execute_disable();
					paging::map::<BasePageSize>(IOAPIC_ADDRESS, ioapic_record.address as usize, 1, flags);
				}
			},
			_ => {
				// Just ignore other entries for now.
			}
		}

		current_address += record.length as usize - mem::size_of::<AcpiMadtRecordHeader>();
	}

	// Successfully derived all information from the MADT.
	// Return the physical address of the Local APIC.
	Ok(madt_header.local_apic_address as usize)
}

fn detect_from_uhyve() -> Result<usize, ()> {
	if environment::is_uhyve() {
		return Ok(0xFEE00000 as usize);
	}

	Err(())
}

#[no_mangle]
pub extern "C" fn eoi() {
	local_apic_write(IA32_X2APIC_EOI, APIC_EOI_ACK);
}

pub fn init() {
	// Initialize an empty vector for the Local APIC IDs of all CPUs.
	unsafe { CPU_LOCAL_APIC_IDS = Some(Vec::new()); }

	// Detect CPUs and APICs.
	let local_apic_physical_address = detect_from_uhyve()
		.or_else(|_e| detect_from_acpi())
		.expect("HermitCore requires an APIC system");

	// Initialize x2APIC or xAPIC, depending on what's available.
	init_x2apic();
	if !processor::supports_x2apic() {
		// We use the traditional xAPIC mode available on all x86-64 CPUs.
		// It uses a mapped page for communication.
		unsafe {
			LOCAL_APIC_ADDRESS = virtualmem::allocate(BasePageSize::SIZE);
			debug!("Mapping Local APIC at {:#X} to virtual address {:#X}", local_apic_physical_address, LOCAL_APIC_ADDRESS);

			let mut flags = PageTableEntryFlags::empty();
			flags.device().writable().execute_disable();
			paging::map::<BasePageSize>(LOCAL_APIC_ADDRESS, local_apic_physical_address, 1, flags);
		}
	}

	// Set gates to ISRs for the APIC interrupts we are going to enable.
	idt::set_gate(TLB_FLUSH_INTERRUPT_NUMBER, tlb_flush_handler as usize, 0);
	idt::set_gate(ERROR_INTERRUPT_NUMBER, error_interrupt_handler as usize, 0);
	idt::set_gate(SPURIOUS_INTERRUPT_NUMBER, spurious_interrupt_handler as usize, 0);
	idt::set_gate(WAKEUP_INTERRUPT_NUMBER, wakeup_handler as usize, 0);

	// Initialize interrupt handling over APIC.
	// All interrupts of the PIC have already been masked, so it doesn't need to be disabled again.
	init_local_apic();

	if !processor::supports_tsc_deadline() {
		// We have an older APIC Timer without TSC Deadline support, which has a maximum timeout
		// and needs to be calibrated.
		calibrate_timer();
	}

	// init ioapic
	if !environment::is_uhyve() {
		init_ioapic();
	}
}

fn init_ioapic() {
	let max_entry = ioapic_max_redirection_entry()+1;
	info!("IOAPIC v{} has {} entries", ioapic_version(), max_entry);

	// now lets turn everything else on
	for i in 0..max_entry {
		if i != 2 {
			ioapic_inton(i, 0 /*apic_processors[boot_processor]->id*/).unwrap();
		} else {
			// now, we don't longer need the IOAPIC timer and turn it off
			info!("Disable IOAPIC timer");
			ioapic_intoff(2, 0 /*apic_processors[boot_processor]->id*/).unwrap();
		}
	}
}

fn ioapic_inton(irq: u8, apicid: u8) -> Result<(), ()>
{
	if irq > 24 {
		error!("IOAPIC: trying to turn on irq {} which is too high\n", irq);
		return Err(());
	}

	let off = (irq*2) as u32;
	let ioredirect_upper: u32 = (apicid as u32) << 24;
	let ioredirect_lower: u32 = (0x20+irq) as u32;

	ioapic_write(IOAPIC_REG_TABLE+off, ioredirect_lower);
	ioapic_write(IOAPIC_REG_TABLE+1+off, ioredirect_upper);

	Ok(())
}

fn ioapic_intoff(irq: u32, apicid: u32) -> Result<(), ()>
{
	if irq > 24 {
		error!("IOAPIC: trying to turn off irq {} which is too high\n", irq);
		return Err(());
	}

	let off = (irq*2) as u32;
	let ioredirect_upper: u32 = (apicid as u32) << 24;
	let ioredirect_lower: u32 = ((0x20+irq) as u32) | (1 << 16); // turn it off (start masking)

	ioapic_write(IOAPIC_REG_TABLE+off, ioredirect_lower);
	ioapic_write(IOAPIC_REG_TABLE+1+off, ioredirect_upper);

	Ok(())
}

pub fn init_local_apic() {
	// Mask out all interrupts we don't need right now.
	local_apic_write(IA32_X2APIC_LVT_TIMER, APIC_LVT_MASK);
	local_apic_write(IA32_X2APIC_LVT_THERMAL, APIC_LVT_MASK);
	local_apic_write(IA32_X2APIC_LVT_PMI, APIC_LVT_MASK);
	local_apic_write(IA32_X2APIC_LVT_LINT0, APIC_LVT_MASK);
	local_apic_write(IA32_X2APIC_LVT_LINT1, APIC_LVT_MASK);

	// Set the interrupt number of the Error interrupt.
	local_apic_write(IA32_X2APIC_LVT_ERROR, ERROR_INTERRUPT_NUMBER as u64);

	// allow all interrupts
	local_apic_write(IA32_X2APIC_TPR, 0x00);

	// Finally, enable the Local APIC by setting the interrupt number for spurious interrupts
	// and providing the enable bit.
	local_apic_write(IA32_X2APIC_SIVR, APIC_SIVR_ENABLED | (SPURIOUS_INTERRUPT_NUMBER as u64));
}

fn calibrate_timer() {
	// The APIC Timer is used to provide a one-shot interrupt for the tickless timer
	// implemented through processor::get_timer_ticks.
	// Therefore determine a counter value for 1 microsecond, which is the resolution
	// used throughout all of HermitCore. Wait 30ms for accuracy.
	let microseconds = 30_000;

	// Disable interrupts for calibration accuracy and initialize the counter.
	// Dividing the counter value by 8 still provides enough accuracy for 1 microsecond resolution,
	// but allows for longer timeouts than a smaller divisor.
	// For example, on an Intel Xeon E5-2650 v3 @ 2.30GHz, the counter is usually calibrated to
	// 125, which allows for timeouts of approximately 34 seconds (u32::MAX / 125).
	irq::disable();
	local_apic_write(IA32_X2APIC_DIV_CONF, APIC_DIV_CONF_DIVIDE_BY_8);
	local_apic_write(IA32_X2APIC_INIT_COUNT, u32::MAX as u64);

	// Wait until the calibration time has elapsed.
	processor::udelay(microseconds);

	// Save the difference of the initial value and current value as the result of the calibration
	// and reenable interrupts.
	unsafe {
		CALIBRATED_COUNTER_VALUE = ((u32::MAX - local_apic_read(IA32_X2APIC_CUR_COUNT)) as u64) / microseconds;
		debug!("Calibrated APIC Timer with a counter value of {} for 1 microsecond", CALIBRATED_COUNTER_VALUE);
	}
	irq::enable();
}

pub fn set_oneshot_timer(wakeup_time: Option<u64>) {
	if let Some(wt) = wakeup_time {
		if processor::supports_tsc_deadline() {
			// wt is the absolute wakeup time in microseconds based on processor::get_timer_ticks.
			// We can simply multiply it by the processor frequency to get the absolute Time-Stamp Counter deadline
			// (see processor::get_timer_ticks).
			let tsc_deadline = wt * (processor::get_frequency() as u64);

			// Enable the APIC Timer in TSC-Deadline Mode and let it start by writing to the respective MSR.
			local_apic_write(IA32_X2APIC_LVT_TIMER, APIC_LVT_TIMER_TSC_DEADLINE | TIMER_INTERRUPT_NUMBER as u64);
			unsafe { wrmsr(IA32_TSC_DEADLINE, tsc_deadline); }
		} else {
			// Calculate the relative timeout from the absolute wakeup time.
			// Maintain a minimum value of one tick, otherwise the timer interrupt does not fire at all.
			// The Timer Counter Register is also a 32-bit register, which we must not overflow for longer timeouts.
			let current_time = processor::get_timer_ticks();
			let ticks = if wt > current_time { wt - current_time } else { 1 };
			let init_count = cmp::min(unsafe { CALIBRATED_COUNTER_VALUE } * ticks, u32::MAX as u64);

			// Enable the APIC Timer in One-Shot Mode and let it start by setting the initial counter value.
			local_apic_write(IA32_X2APIC_LVT_TIMER, TIMER_INTERRUPT_NUMBER as u64);
			local_apic_write(IA32_X2APIC_INIT_COUNT, init_count);
		}
	} else {
		// Disable the APIC Timer.
		local_apic_write(IA32_X2APIC_LVT_TIMER, APIC_LVT_MASK);
	}
}

pub fn init_x2apic() {
	if processor::supports_x2apic() {
		// The CPU supports the modern x2APIC mode, which uses MSRs for communication.
		// Enable it.
		let mut apic_base = unsafe { rdmsr(IA32_APIC_BASE) };
		apic_base |= X2APIC_ENABLE;
		unsafe { wrmsr(IA32_APIC_BASE, apic_base); }
	}
}

/// Initialize the required entry.asm variables for the next CPU to be booted.
pub fn init_next_processor_variables(core_id: usize) {
	// Allocate stack and PerCoreVariables structure for the CPU and pass the addresses.
	// Keep the stack executable to possibly support dynamically generated code on the stack (see https://security.stackexchange.com/a/47825).
	let stack = mm::allocate(KERNEL_STACK_SIZE, false);
	let boxed_percore = Box::new(PerCoreVariables::new(core_id));
	unsafe {
		ptr::write_volatile(&mut KERNEL_HEADER.current_stack_address, stack as u64);
		ptr::write_volatile(&mut KERNEL_HEADER.current_percore_address, Box::into_raw(boxed_percore) as u64);
	}
}

/// Boot all Application Processors
/// This algorithm is derived from Intel MultiProcessor Specification 1.4, B.4, but testing has shown
/// that a second STARTUP IPI and setting the BIOS Reset Vector are no longer necessary.
/// This is partly confirmed by https://wiki.osdev.org/Symmetric_Multiprocessing
pub fn boot_application_processors() {
	// We shouldn't have any problems fitting the boot code into a single page, but let's better be sure.
	assert!(SMP_BOOT_CODE.len() < BasePageSize::SIZE, "SMP Boot Code is larger than a page");
	debug!("SMP boot code is {} bytes long", SMP_BOOT_CODE.len());

	// Identity-map the boot code page and copy over the code.
	debug!("Mapping SMP boot code to physical and virtual address {:#X}", SMP_BOOT_CODE_ADDRESS);
	let mut flags = PageTableEntryFlags::empty();
	flags.normal().writable();
	paging::map::<BasePageSize>(SMP_BOOT_CODE_ADDRESS, SMP_BOOT_CODE_ADDRESS, 1, flags);
	unsafe { ptr::copy_nonoverlapping(&SMP_BOOT_CODE as *const u8, SMP_BOOT_CODE_ADDRESS as *mut u8, SMP_BOOT_CODE.len()); }

	unsafe {
		// Pass the PML4 page table address to the boot code.
		*((SMP_BOOT_CODE_ADDRESS + SMP_BOOT_CODE_OFFSET_PML4) as *mut u32) = cr3() as u32;
		// Set entry point
		debug!("Set entry point for application processor to 0x{:x}", arch::x86_64::kernel::start::_start as u64);
		*((SMP_BOOT_CODE_ADDRESS + SMP_BOOT_CODE_OFFSET_ENTRY) as *mut u64) = arch::x86_64::kernel::start::_start as u64;
	}

	// Now wake up each application processor.
	let apic_ids = unsafe { CPU_LOCAL_APIC_IDS.as_ref().unwrap() };
	let core_id = core_id();

	for core_id_to_boot in 0..apic_ids.len() {
		if core_id_to_boot != core_id {
			let apic_id = apic_ids[core_id_to_boot];
			let destination = (apic_id as u64) << 32;

			debug!("Waking up CPU {} with Local APIC ID {}", core_id_to_boot, apic_id);
			init_next_processor_variables(core_id_to_boot);

			// Save the current number of initialized CPUs.
			let current_processor_count = arch::get_processor_count();

			// Send an INIT IPI.
			local_apic_write(IA32_X2APIC_ICR, destination | APIC_ICR_LEVEL_TRIGGERED | APIC_ICR_LEVEL_ASSERT | APIC_ICR_DELIVERY_MODE_INIT);
			processor::udelay(200);

			local_apic_write(IA32_X2APIC_ICR, destination | APIC_ICR_LEVEL_TRIGGERED | APIC_ICR_DELIVERY_MODE_INIT);
			processor::udelay(10000);

			// Send a STARTUP IPI.
			local_apic_write(IA32_X2APIC_ICR, destination | APIC_ICR_DELIVERY_MODE_STARTUP | ((SMP_BOOT_CODE_ADDRESS as u64) >> 12));
			debug!("Waiting for it to respond");

			// Wait until the application processor has finished initializing.
			// It will indicate this by counting up cpu_online.
			while current_processor_count == arch::get_processor_count() {
				processor::udelay(1000);
			}
		}
	}
}

pub fn ipi_tlb_flush() {
	if arch::get_processor_count() > 1 {
		let apic_ids = unsafe { CPU_LOCAL_APIC_IDS.as_ref().unwrap() };
		let core_id = core_id();

		// Ensure that all memory operations have completed before issuing a TLB flush.
		unsafe { asm!("mfence" ::: "memory" : "volatile"); }

		// Send an IPI with our TLB Flush interrupt number to all other CPUs.
		for core_id_to_interrupt in 0..apic_ids.len() {
			if core_id_to_interrupt != core_id {
				let local_apic_id = apic_ids[core_id_to_interrupt];
				let destination = (local_apic_id as u64) << 32;
				local_apic_write(IA32_X2APIC_ICR, destination | APIC_ICR_LEVEL_ASSERT | APIC_ICR_DELIVERY_MODE_FIXED | (TLB_FLUSH_INTERRUPT_NUMBER as u64));
			}
		}
	}
}

/// Send an inter-processor interrupt to wake up a CPU Core that is in a HALT state.
pub fn wakeup_core(core_id_to_wakeup: usize) {
	if core_id_to_wakeup != core_id() {
		let apic_ids = unsafe { CPU_LOCAL_APIC_IDS.as_ref().unwrap() };
		let local_apic_id = apic_ids[core_id_to_wakeup];
		let destination = (local_apic_id as u64) << 32;
		local_apic_write(IA32_X2APIC_ICR, destination | APIC_ICR_LEVEL_ASSERT | APIC_ICR_DELIVERY_MODE_FIXED | (WAKEUP_INTERRUPT_NUMBER as u64));
	}
}

/// Translate the x2APIC MSR into an xAPIC memory address.
#[inline]
fn translate_x2apic_msr_to_xapic_address(x2apic_msr: u32) -> usize {
	unsafe { LOCAL_APIC_ADDRESS + ((x2apic_msr as usize & 0xFF) << 4) }
}

fn local_apic_read(x2apic_msr: u32) -> u32 {
	if processor::supports_x2apic() {
		// x2APIC is simple, we can just read from the given MSR.
		unsafe { rdmsr(x2apic_msr) as u32 }
	} else {
		unsafe { *(translate_x2apic_msr_to_xapic_address(x2apic_msr) as *const u32) }
	}
}

fn ioapic_write(reg: u32, value: u32)
{
	unsafe {
		ptr::write_volatile(IOAPIC_ADDRESS as *mut u32, reg);
		ptr::write_volatile((IOAPIC_ADDRESS + 4*mem::size_of::<u32>()) as *mut u32, value);
	}
}

fn ioapic_read(reg: u32) -> u32
{
	let value;

	unsafe {
		ptr::write_volatile(IOAPIC_ADDRESS as *mut u32, reg);
		value = ptr::read_volatile((IOAPIC_ADDRESS + 4*mem::size_of::<u32>()) as *const u32);
	}

	value
}

fn ioapic_version() -> u32
{
	ioapic_read(IOAPIC_REG_VER) & 0xFF
}

fn ioapic_max_redirection_entry() -> u8
{
	((ioapic_read(IOAPIC_REG_VER) >> 16) & 0xFF) as u8
}

fn local_apic_write(x2apic_msr: u32, value: u64) {
	if processor::supports_x2apic() {
		// x2APIC is simple, we can just write the given value to the given MSR.
		unsafe { wrmsr(x2apic_msr, value); }
	} else {
		if x2apic_msr == IA32_X2APIC_ICR {
			// Instead of a single 64-bit ICR register, xAPIC has two 32-bit registers (ICR1 and ICR2).
			// There is a gap between them and the destination field in ICR2 is also 8 bits instead of 32 bits.
			let destination = ((value >> 8) & 0xFF00_0000) as u32;
			let icr2 = unsafe { &mut *((LOCAL_APIC_ADDRESS + APIC_ICR2) as *mut u32) };
			*icr2 = destination;

			// The remaining data without the destination will now be written into ICR1.
		}

		// Write the value.
		let value_ref = unsafe { &mut *(translate_x2apic_msr_to_xapic_address(x2apic_msr) as *mut u32) };
		*value_ref = value as u32;

		if x2apic_msr == IA32_X2APIC_ICR {
			// The ICR1 register in xAPIC mode also has a Delivery Status bit that must be checked.
			// Wait until the CPU clears it.
			// This bit does not exist in x2APIC mode (cf. Intel Vol. 3A, 10.12.9).
			while (unsafe { ptr::read_volatile(value_ref) } & APIC_ICR_DELIVERY_STATUS_PENDING) > 0 {
				spin_loop_hint();
			}
		}
	}
}

pub fn print_information() {
	infoheader!(" MULTIPROCESSOR INFORMATION ");
	infoentry!("APIC in use", if processor::supports_x2apic() { "x2APIC" } else { "xAPIC" });
	infoentry!("Initialized CPUs", arch::get_processor_count());
	infofooter!();
}