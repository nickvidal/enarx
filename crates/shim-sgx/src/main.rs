// SPDX-License-Identifier: Apache-2.0

//! The SGX shim
//!
//! This crate contains the system that traps the syscalls (and cpuid
//! instructions) from the enclave code and proxies them to the host.

#![no_std]
#![feature(asm_const, naked_functions)]
#![deny(clippy::all)]
#![deny(missing_docs)]
#![warn(rust_2018_idioms)]
#![no_main]

#[allow(unused_extern_crates)]
extern crate rcrt1;

use core::arch::asm;
use core::mem::MaybeUninit;
use core::ptr::NonNull;

use enarx_shim_sgx::thread::{
    LoadRegsExt, NewThread, NewThreadFromRegisters, Tcb, NEW_THREAD_QUEUE, THREADS_FREE,
};
use enarx_shim_sgx::{
    entry, handler, shim_address, ATTR, BLOCK_SIZE, CSSA_0_STACK_SIZE, ENARX_EXEC_START,
    ENARX_SHIM_ADDRESS, ENCL_SIZE, ENCL_SIZE_BITS, MISC,
};

#[panic_handler]
#[cfg(not(test))]
#[allow(clippy::empty_loop)]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}

// ============== REAL CODE HERE ===============

use noted::noted;
use primordial::Page;
use sallyport::{elf::note, REQUIRES};
use sgx::parameters::{Attributes, MiscSelect};
use sgx::ssa::{GenPurposeRegs, StateSaveArea};

noted! {
    static NOTE_REQUIRES<note::NAME, note::REQUIRES, [u8; REQUIRES.len()]> = REQUIRES;

    static NOTE_BLOCK_SIZE<note::NAME, note::BLOCK_SIZE, u64> = BLOCK_SIZE as u64;

    static NOTE_BITS<note::NAME, note::sgx::BITS, u8> = ENCL_SIZE_BITS;
    static NOTE_SSAP<note::NAME, note::sgx::SSAP, u8> = 1;

    static NOTE_PID<note::NAME, note::sgx::PID, u16> = 0;
    static NOTE_SVN<note::NAME, note::sgx::SVN, u16> = 0;
    static NOTE_MISC<note::NAME, note::sgx::MISC, MiscSelect> = MISC;
    static NOTE_MISCMASK<note::NAME, note::sgx::MISCMASK, MiscSelect> = MISC;
    static NOTE_ATTR<note::NAME, note::sgx::ATTR, Attributes> = ATTR;
    static NOTE_ATTRMASK<note::NAME, note::sgx::ATTRMASK, Attributes> = ATTR;
}

/// Clear CPU flags, extended state and temporary registers (`r10` and `r11`)
///
/// This function clears CPU state during enclave transitions.
///
/// # Safety
///
/// This function should be safe as it only modifies non-preserved
/// registers. In fact, in addition to the declared calling convention,
/// we promise not to modify any of the parameter registers.
#[naked]
extern "sysv64" fn clearx() {
    use const_default::ConstDefault;
    static XSAVE: xsave::XSave = <xsave::XSave as ConstDefault>::DEFAULT;

    unsafe {
        asm!(
            // Clear all temporary registers
            "xor    r10,    r10",
            "xor    r11,    r11",

            // Clear CPU state bits and DF/AC flags
            // Note: we can simply popfq an all-zero value, as system flags and
            // reserved bits are not writable from the user-space enclave
            "push    QWORD PTR 0",
            "popfq",

            // Clear the extended CPU state
            "push    rax            ",  // Save rax
            "push    rdx            ",  // Save rdx
            "mov     rdx,   ~0      ",  // Set mask for xrstor in rdx
            "mov     rax,   ~0      ",  // Set mask for xrstor in rax
            "xrstor  [rip + {XSAVE}]",  // Clear xCPU state with synthetic state
            "pop     rdx            ",  // Restore rdx
            "pop     rax            ",  // Restore rax

            "ret",

            XSAVE = sym XSAVE,
            options(noreturn)
        )
    }
}

/// Clears parameter registers
///
/// # Safety
///
/// This function should be safe as it only modifies non-preserved
/// registers. It really doesn't even need to be a naked function
/// except that Rust tries really hard to put `rax` on the stack
/// and then pops it off into a random register (usually `rcx`).
#[naked]
extern "sysv64" fn clearp() {
    unsafe {
        asm!(
            "xor    rax,    rax",
            "xor    rdi,    rdi",
            "xor    rsi,    rsi",
            "xor    rdx,    rdx",
            "xor    rcx,    rcx",
            "xor    r8,     r8",
            "xor    r9,     r9",
            "ret",
            options(noreturn)
        )
    }
}

/// Perform relocation
///
/// # Safety
///
/// This function does not follow any established calling convention. It
/// has the following requirements:
///   * `rsp` must point to a stack with the return address (i.e. `call`)
///
/// Upon return, all general-purpose registers will have been preserved.
#[naked]
unsafe extern "sysv64" fn relocate() {
    asm!(
        "push   rax",
        "push   rdi",
        "push   rsi",
        "push   rdx",
        "push   rcx",
        "push   r8",
        "push   r9",
        "push   r10",
        "push   r11",

        "lea    rdi,    [rip + _DYNAMIC]",             // rdi = address of _DYNAMIC section
        "lea    rsi,    [rip + {ENARX_SHIM_ADDRESS}]", // rsi = enclave start address mask
        "call   {DYN_RELOC}",                          // relocate the dynamic symbols

        "pop    r11",
        "pop    r10",
        "pop    r9",
        "pop    r8",
        "pop    rcx",
        "pop    rdx",
        "pop    rsi",
        "pop    rdi",
        "pop    rax",

        "ret",

        DYN_RELOC = sym rcrt1::dyn_reloc,
        ENARX_SHIM_ADDRESS = sym ENARX_SHIM_ADDRESS,
        options(noreturn)
    )
}

/// Entry point
///
/// This function is called during EENTER. Its inputs are as follows:
///
///  rax = The current SSA index. (i.e. rbx->cssa)
///  rbx = The address of the TCS.
///  rcx = The next address after the EENTER instruction.
///
/// If rax == 0, we are doing normal execution.
/// Otherwise, we are handling an exception.
///
/// # Safety
///
/// Do not call this function from Rust. It is the entry point for SGX.
#[naked]
#[no_mangle]
pub unsafe extern "sysv64" fn _start() -> ! {
    use core::mem::size_of;

    asm!(
        "cld                                ",  // Clear Direction Flag
        "xchg   rbx,    rcx                 ",  // rbx = exit address, rcx = TCS page

        // Find stack pointer for CSSA == 0
        "cmp    rax,    0                   ",  // If CSSA > 0
        "jne    2f                          ",  // ... jump to the next section
        "mov    r10,    rcx                 ",  // r10 = TCS page
        "sub    r10,    4096                ",  // r10 = skip TCB page = stack pointer
        "jmp    4f                          ",  // Jump to stack setup

        // Get the address of the previous SSA
        "2:                                 ",
        "mov    r10,    rax                 ",  // r10 = CSSA
        "shl    r10,    12                  ",  // r10 = CSSA * 4096
        "add    r10,    rcx                 ",  // r10 = &SSA[CSSA - 1]

        // Determine if exceptions were enabled
        "mov    r11,    [r10 + {EXTO}]      ",  // r11 = SSA[CSSA - 1].extra[0]
        "cmp    r11,    0                   ",  // If exceptions aren't enabled yet...
        "je     2b                          ",  // ... loop forever.

        // Find stack pointer for CSSA == 1
        "cmp    rax,    1                   ",  // If CSSA > 1
        "jne    3f                          ",  // ... jump to the next section
        "mov    r10,    rcx                 ",  // r10 = stack pointer CSSA == 1
        "sub    r10,    {CSSA_0_STK_TCS_SZ} ",  // r10 = stack pointer - CSSA_0_STK_TCS_SZ
        "jmp    4f                          ",  // Jump to stack setup

        // Find stack pointer for CSSA > 1
        "3:                                 ",
        "mov    r10,    [r10 + {RSPO}]      ",  // r10 = SSA[CSSA - 1].gpr.rsp
        "sub    r10,    128                 ",  // Skip the red zone

        // Setup the stack
        "4:                                 ",
        "and    r10,    ~0xf                ",  // Align the stack
        "xchg   rsp,    r10                 ",  // Swap r10 and rsp
        "sub    rsp,    8                   ",  // Align the stack
        "push   r10                         ",  // Store old stack

        // Do relocation if CSSA == 0
        "cmp    rax,    0                   ",  // If CSSA > 0
        "jne    5f                          ",  // ... jump to the next section
        "call   {RELOC}                     ",  // Relocate symbols

        // Clear, call Rust, clear
        "5:                                 ",  // rdi = &mut sallyport::Block (passthrough)
        "lea    rsi,    [rcx + 4096]        ",  // rsi = &mut [StateSaveArea; N]
        "mov    rdx,    rax                 ",  // rdx = CSSA
        "sub    rcx,    4096                ",  // rcx = TCB
        "call   {CLEARX}                    ",  // Clear CPU state
        "call   {ENTRY}                     ",  // Jump to Rust
        "push   rax                         ",  // Save return value
        "call   {CLEARX}                    ",  // Clear CPU state
        "call   {CLEARP}                    ",  // Clear parameter registers
        "pop    r8                          ",  // Restore return value

        // Exit
        "pop    rsp                         ",  // Restore old stack
        "mov    rax,    {EEXIT}             ",  // rax = EEXIT
        "enclu                              ",  // Exit enclave

        // offset_of!(StateSaveArea, gpr.rsp)
        RSPO = const size_of::<StateSaveArea>() - size_of::<GenPurposeRegs>() + 32,

        // offset_of!(StateSaveArea, extra)
        EXTO = const size_of::<xsave::XSave>(),

        CLEARX = sym clearx,
        CLEARP = sym clearp,
        RELOC = sym relocate,
        ENTRY = sym main,
        EEXIT = const sgx::enclu::EEXIT,
        CSSA_0_STK_TCS_SZ = const CSSA_0_STACK_SIZE + Page::SIZE,
        options(noreturn)
    )
}

unsafe extern "C" fn main(
    block: &mut [usize; BLOCK_SIZE / core::mem::size_of::<usize>()],
    ssas: &mut [StateSaveArea; 3],
    cssa: usize,
    tcb: &mut MaybeUninit<Tcb>,
) -> i32 {
    // Enable exceptions:
    ssas[cssa].extra[0] = 1;

    // As host is a separate application from the shim, all the data coming from
    // it needs to be validated explicitly.  Thus, check that the Sallyport
    // block is outside the shim address space:
    let block_start = block.as_ptr() as usize;
    let block_end = block_start + BLOCK_SIZE;
    let shim_start = shim_address();
    let shim_end = shim_start + ENCL_SIZE;

    if (block_start >= shim_start && block_start < shim_end)
        || (block_end > shim_start && block_end <= shim_end)
    {
        panic!();
    }

    let mut ret = 0;

    match cssa {
        0 => {
            // Initialize the TCB.
            let tcb = {
                tcb.write(Tcb::default());
                tcb.assume_init_mut()
            };

            let thread = { NEW_THREAD_QUEUE.write().pop().unwrap() };

            match thread {
                NewThread::Main => {
                    // register the main thread
                    tcb.tid = 0;

                    // run the executable payload
                    ret = entry::entry(&ENARX_EXEC_START as *const u8 as _, tcb)
                }
                NewThread::Thread(NewThreadFromRegisters {
                    tid,
                    clear_on_exit,
                    regs,
                }) => {
                    // register the thread
                    tcb.tid = tid;
                    tcb.clear_on_exit = NonNull::new(clear_on_exit as _);

                    // load the registers
                    ret = regs.load_registers(tcb);
                }
            }
            // increment the free counter, although it's not yet completely done
            *THREADS_FREE.write() += 1;
        }
        1 => {
            // cssa == 0 already initialized the TCB
            let tcb = tcb.assume_init_mut();
            handler::Handler::handle(
                &mut ssas[0],
                block.as_mut_slice(),
                tcb,
                _start as usize as _,
            )
        }
        n => {
            let tcb = tcb.assume_init_mut();
            handler::Handler::finish(&mut ssas[n - 1], block.as_mut_slice(), tcb)
        }
    }

    // Disable exceptions:
    ssas[cssa].extra[0] = 0;
    ret
}
