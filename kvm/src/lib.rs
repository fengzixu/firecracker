// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A safe wrapper around the kernel's KVM interface.

extern crate libc;
extern crate byteorder;
extern crate kvm_sys;
#[macro_use]
extern crate sys_util;

mod cap;

use std::fs::File;
use std::os::raw::*;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

use libc::{open, O_RDWR, EINVAL, ENOSPC};

use kvm_sys::*;

use sys_util::{MemoryMapping, EventFd, Result, Error, errno_result};
use sys_util::{ioctl, ioctl_with_val, ioctl_with_ref, ioctl_with_mut_ref, ioctl_with_ptr,
               ioctl_with_mut_ptr};

pub use cap::*;

/// A wrapper around opening and using `/dev/kvm`.
///
/// Useful for querying extensions and basic values from the KVM backend. A `Kvm` is required to
/// create a `Vm` object.
pub struct Kvm {
    kvm: File,
}

impl Kvm {
    /// Opens `/dev/kvm/` and returns a Kvm object on success.
    pub fn new() -> Result<Kvm> {
        // Open calls are safe because we give a constant nul-terminated string and verify the
        // result.
        let ret = unsafe { open("/dev/kvm\0".as_ptr() as *const c_char, O_RDWR) };
        if ret < 0 {
            return errno_result();
        }
        // Safe because we verify that ret is valid and we own the fd.
        Ok(Kvm { kvm: unsafe { File::from_raw_fd(ret) } })
    }

    /// Query the availability of a particular kvm capability
    fn check_extension_int(&self, c: Cap) -> i32 {
        // Safe because we know that our file is a KVM fd and that the extension is one of the ones
        // defined by kernel.
        unsafe { ioctl_with_val(self, KVM_CHECK_EXTENSION(), c as c_ulong) }
    }

    /// Checks if a particular `Cap` is available.
    pub fn check_extension(&self, c: Cap) -> bool {
        self.check_extension_int(c) == 1
    }

    /// Gets the size of the mmap required to use vcpu's `kvm_run` structure.
    pub fn get_vcpu_mmap_size(&self) -> Result<usize> {
        // Safe because we know that our file is a KVM fd and we verify the return result.
        let res = unsafe { ioctl(self, KVM_GET_VCPU_MMAP_SIZE() as c_ulong) };
        if res > 0 {
            Ok(res as usize)
        } else {
            errno_result()
        }
    }

    /// Gets the recommended maximum number of VCPUs per VM.
    pub fn get_nr_vcpus(&self) -> usize {
        match self.check_extension_int(Cap::NrVcpus) {
            0 => 4, // according to api.txt
            x if x > 0 => x as usize,
            _ => {
                warn!("kernel returned invalid number of VCPUs");
                4
            }
        }
    }

    /// X86 specific call to get the system supported CPUID values
    ///
    /// # Arguments
    ///
    /// * `max_cpus` - Maximum number of cpuid entries to return.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn get_supported_cpuid(&self, max_cpus: usize) -> Result<CpuId> {
        let mut cpuid = CpuId::new(max_cpus);

        let ret = unsafe {
            // ioctl is unsafe. The kernel is trusted not to write beyond the bounds of the memory
            // allocated for the struct. The limit is read from nent, which is set to the allocated
            // size(max_cpus) above.
            ioctl_with_mut_ptr(self, KVM_GET_SUPPORTED_CPUID(), cpuid.as_mut_ptr())
        };
        if ret < 0 {
            return errno_result();
        }

        Ok(cpuid)
    }
}

impl AsRawFd for Kvm {
    fn as_raw_fd(&self) -> RawFd {
        self.kvm.as_raw_fd()
    }
}


/// A wrapper around creating and using a VM.
pub struct VmFd {
    vm: File,
    cpuid: CpuId,
    run_size: usize,
}

impl VmFd {
    /// Constructs a new `VmFd` using the given `Kvm` instance.
    pub fn new(kvm: &Kvm) -> Result<VmFd> {
        // Safe because we know kvm is a real kvm fd as this module is the only one that can make
        // Kvm objects.
        let ret = unsafe { ioctl(kvm, KVM_CREATE_VM()) };
        if ret >= 0 {
            // Safe because we verify the value of ret and we are the owners of the fd.
            let vm_file = unsafe { File::from_raw_fd(ret) };
            let run_mmap_size = kvm.get_vcpu_mmap_size()?;
            let max_cpus = kvm.get_nr_vcpus();
            let kvm_cpuid: CpuId = kvm.get_supported_cpuid(max_cpus)?;
            Ok(VmFd {
                vm: vm_file,
                cpuid: kvm_cpuid,
                run_size: run_mmap_size,
            })
        } else {
            errno_result()
        }
    }

    /// Returns the size of a VcpuFd mmap area
    pub fn get_run_size(&self) -> usize {
        self.run_size
    }

    /// Returns a clone of the system supported CPUID values associated with this VmFd
    pub fn get_cpuid(&self) -> CpuId {
        self.cpuid.clone()
    }

    /// Creates/modifies a guest physical memory slot
    pub fn set_user_memory_region(
        &self,
        slot: u32,
        guest_addr: u64,
        memory_size: u64,
        userspace_addr: u64,
        flags: u32,
    ) -> Result<()> {
        let region = kvm_userspace_memory_region {
            slot,
            flags: flags,
            guest_phys_addr: guest_addr,
            memory_size,
            userspace_addr,
        };

        let ret = unsafe { ioctl_with_ref(self, KVM_SET_USER_MEMORY_REGION(), &region) };
        if ret == 0 { Ok(()) } else { errno_result() }
    }

    /// Sets the address of the three-page region in the VM's address space.
    ///
    /// See the documentation on the KVM_SET_TSS_ADDR ioctl.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn set_tss_address(&self, offset: usize) -> Result<()> {
        // Safe because we know that our file is a VM fd and we verify the return result.
        let ret = unsafe { ioctl_with_val(self, KVM_SET_TSS_ADDR(), offset as c_ulong) };
        if ret == 0 { Ok(()) } else { errno_result() }
    }

    /// Crates an in kernel interrupt controller.
    ///
    /// See the documentation on the KVM_CREATE_IRQCHIP ioctl.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "arm",
                target_arch = "aarch64"))]
    pub fn create_irq_chip(&self) -> Result<()> {
        // Safe because we know that our file is a VM fd and we verify the return result.
        let ret = unsafe { ioctl(self, KVM_CREATE_IRQCHIP()) };
        if ret == 0 { Ok(()) } else { errno_result() }
    }


    /// Creates a PIT as per the KVM_CREATE_PIT2 ioctl.
    ///
    /// Note that this call can only succeed after a call to `Vm::create_irq_chip`.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn create_pit2(&self) -> Result<()> {
        //TODO: do we need to enable KVM_PIT_SPEAKER_DUMMY?
        let pit_config = kvm_pit_config::default();
        // Safe because we know that our file is a VM fd, we know the kernel will only read the
        // correct amount of memory from our pointer, and we verify the return result.
        let ret = unsafe { ioctl_with_ref(self, KVM_CREATE_PIT2(), &pit_config) };
        if ret == 0 { Ok(()) } else { errno_result() }
    }

    /// Registers an event that will, when signalled, trigger the `gsi` irq.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "arm",
                target_arch = "aarch64"))]
    pub fn register_irqfd(&self, evt: &EventFd, gsi: u32) -> Result<()> {
        let irqfd = kvm_irqfd {
            fd: evt.as_raw_fd() as u32,
            gsi: gsi,
            ..Default::default()
        };
        // Safe because we know that our file is a VM fd, we know the kernel will only read the
        // correct amount of memory from our pointer, and we verify the return result.
        let ret = unsafe { ioctl_with_ref(self, KVM_IRQFD(), &irqfd) };
        if ret == 0 { Ok(()) } else { errno_result() }
    }
}

impl AsRawFd for VmFd {
    fn as_raw_fd(&self) -> RawFd {
        self.vm.as_raw_fd()
    }
}

/// A reason why a VCPU exited.
#[derive(Debug)]
pub enum VcpuExit<'a> {
    /// An out port instruction was run on the given port with the given data.
    IoOut(u16 /* port */, &'a [u8] /* data */),
    /// An in port instruction was run on the given port.
    ///
    /// The given slice should be filled in before `Vcpu::run` is called again.
    IoIn(u16 /* port */, &'a mut [u8] /* data */),
    /// A read instruction was run against the given MMIO address.
    ///
    /// The given slice should be filled in before `Vcpu::run` is called again.
    MmioRead(u64 /* address */, &'a mut [u8]),
    /// A write instruction was run against the given MMIO address with the given data.
    MmioWrite(u64 /* address */, &'a [u8]),
    Unknown,
    Exception,
    Hypercall,
    Debug,
    Hlt,
    IrqWindowOpen,
    Shutdown,
    FailEntry,
    Intr,
    SetTpr,
    TprAccess,
    S390Sieic,
    S390Reset,
    Dcr,
    Nmi,
    InternalError,
    Osi,
    PaprHcall,
    S390Ucontrol,
    Watchdog,
    S390Tsch,
    Epr,
    SystemEvent,
}

/// A wrapper around creating and using a kvm related VCPU fd
pub struct VcpuFd {
    vcpu: File,
    run_mmap: MemoryMapping,
}

impl VcpuFd {
    /// Constructs a new kvm VCPU fd
    ///
    /// The `id` argument is the CPU number between [0, max vcpus).
    pub fn new(id: u8, vm: &VmFd) -> Result<VcpuFd> {
        let run_mmap_size = vm.get_run_size();

        // Safe because we know that vm is a VM fd and we verify the return result.
        let vcpu_fd = unsafe { ioctl_with_val(vm, KVM_CREATE_VCPU(), id as c_ulong) };
        if vcpu_fd < 0 {
            return errno_result();
        }

        // Wrap the vcpu now in case the following ? returns early. This is safe because we verified
        // the value of the fd and we own the fd.
        let vcpu = unsafe { File::from_raw_fd(vcpu_fd) };

        let run_mmap = MemoryMapping::from_fd(&vcpu, run_mmap_size).map_err(|_| {
            Error::new(ENOSPC)
        })?;

        Ok(VcpuFd {
            vcpu: vcpu,
            run_mmap: run_mmap,
        })
    }

    /// Gets the VCPU registers.
    #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
    pub fn get_regs(&self) -> Result<kvm_regs> {
        // Safe because we know that our file is a VCPU fd, we know the kernel will only read the
        // correct amount of memory from our pointer, and we verify the return result.
        let mut regs = unsafe { std::mem::zeroed() };
        let ret = unsafe { ioctl_with_mut_ref(self, KVM_GET_REGS(), &mut regs) };
        if ret != 0 {
            return errno_result();
        }
        Ok(regs)
    }

    /// Sets the VCPU registers.
    #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
    pub fn set_regs(&self, regs: &kvm_regs) -> Result<()> {
        // Safe because we know that our file is a VCPU fd, we know the kernel will only read the
        // correct amount of memory from our pointer, and we verify the return result.
        let ret = unsafe { ioctl_with_ref(self, KVM_SET_REGS(), regs) };
        if ret != 0 {
            return errno_result();
        }
        Ok(())
    }

    /// Gets the VCPU special registers.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn get_sregs(&self) -> Result<kvm_sregs> {
        // Safe because we know that our file is a VCPU fd, we know the kernel will only write the
        // correct amount of memory to our pointer, and we verify the return result.
        let mut regs = kvm_sregs::default();

        let ret = unsafe { ioctl_with_mut_ref(self, KVM_GET_SREGS(), &mut regs) };
        if ret != 0 {
            return errno_result();
        }
        Ok(regs)
    }

    /// Sets the VCPU special registers.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn set_sregs(&self, sregs: &kvm_sregs) -> Result<()> {
        // Safe because we know that our file is a VCPU fd, we know the kernel will only read the
        // correct amount of memory from our pointer, and we verify the return result.
        let ret = unsafe { ioctl_with_ref(self, KVM_SET_SREGS(), sregs) };
        if ret != 0 {
            return errno_result();
        }
        Ok(())
    }

    /// X86 specific call that gets the FPU-related structure
    ///
    /// See the documentation for KVM_GET_FPU.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn get_fpu(&self) -> Result<kvm_fpu> {
        let mut fpu = kvm_fpu::default();

        let ret = unsafe {
            // Here we trust the kernel not to read past the end of the kvm_fpu struct.
            ioctl_with_mut_ref(self, KVM_GET_FPU(), &mut fpu)
        };
        if ret != 0 {
            return errno_result();
        }
        Ok(fpu)
    }

    /// X86 specific call to setup the FPU
    ///
    /// See the documentation for KVM_SET_FPU.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn set_fpu(&self, fpu: &kvm_fpu) -> Result<()> {
        let ret = unsafe {
            // Here we trust the kernel not to read past the end of the kvm_fpu struct.
            ioctl_with_ref(self, KVM_SET_FPU(), fpu)
        };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }

    /// X86 specific call to setup the CPUID registers
    ///
    /// See the documentation for KVM_SET_CPUID2.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn set_cpuid2(&self, cpuid: &CpuId) -> Result<()> {
        let ret = unsafe {
            // Here we trust the kernel not to read past the end of the kvm_msrs struct.
            ioctl_with_ptr(self, KVM_SET_CPUID2(), cpuid.as_ptr())
        };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }

    /// X86 specific call to get the state of the
    /// "Local Advanced Programmable Interrupt Controller".
    ///
    /// See the documentation for KVM_GET_LAPIC.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn get_lapic(&self) -> Result<kvm_lapic_state> {
        let mut klapic = kvm_lapic_state::default();

        let ret = unsafe {
            // The ioctl is unsafe unless you trust the kernel not to write past the end of the
            // local_apic struct.
            ioctl_with_mut_ref(self, KVM_GET_LAPIC(), &mut klapic)
        };
        if ret < 0 {
            return errno_result();
        }
        Ok(klapic)
    }

    /// X86 specific call to set the state of the
    /// "Local Advanced Programmable Interrupt Controller".
    ///
    /// See the documentation for KVM_SET_LAPIC.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn set_lapic(&self, klapic: &kvm_lapic_state) -> Result<()> {
        let ret = unsafe {
            // The ioctl is safe because the kernel will only read from the klapic struct.
            ioctl_with_ref(self, KVM_SET_LAPIC(), klapic)
        };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }

    /// X86 specific call to setup the FPU
    ///
    /// See the documentation for KVM_GET_FPU.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn get_msrs(&self) -> Result<(kvm_msrs)> {
        let mut msrs = kvm_msrs::default();
        let ret = unsafe {
            // Here we trust the kernel not to read past the end of the kvm_msrs struct.
            ioctl_with_mut_ref(self, KVM_GET_MSRS(), &mut msrs)
        };
        if ret != 0 {
            return errno_result();
        }
        Ok(msrs)
    }

    /// X86 specific call to setup the MSRS
    ///
    /// See the documentation for KVM_SET_MSRS.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub fn set_msrs(&self, msrs: &kvm_msrs) -> Result<()> {
        let ret = unsafe {
            // Here we trust the kernel not to read past the end of the kvm_msrs struct.
            ioctl_with_ref(self, KVM_SET_MSRS(), msrs)
        };
        if ret < 0 {
            // KVM_SET_MSRS actually returns the number of msr entries written.
            return errno_result();
        }
        Ok(())
    }

    /// Returns a reference to the kvm_run structure obtained by mmap-ing the associated VcpuFd
    fn get_run(&self) -> &mut kvm_run {
        // Safe because we know we mapped enough memory to hold the kvm_run struct because the
        // kernel told us how large it was.
        unsafe { &mut *(self.run_mmap.as_ptr() as *mut kvm_run) }
    }

    /// Triggers the running of the current virtual CPU returning an exit reason
    pub fn run(&self) -> Result<VcpuExit> {
        // Safe because we know that our file is a VCPU fd and we verify the return result.
        let ret = unsafe { ioctl(self, KVM_RUN()) };
        if ret == 0 {
            let run = self.get_run();
            match run.exit_reason {
                KVM_EXIT_IO => {
                    let run_start = run as *mut kvm_run as *mut u8;
                    // Safe because the exit_reason (which comes from the kernel) told us which
                    // union field to use.
                    let io = unsafe { run.__bindgen_anon_1.io.as_ref() };
                    let port = io.port;
                    let data_size = io.count as usize * io.size as usize;
                    // The data_offset is defined by the kernel to be some number of bytes into the
                    // kvm_run stucture, which we have fully mmap'd.
                    let data_ptr = unsafe { run_start.offset(io.data_offset as isize) };
                    // The slice's lifetime is limited to the lifetime of this Vcpu, which is equal
                    // to the mmap of the kvm_run struct that this is slicing from
                    let data_slice = unsafe {
                        std::slice::from_raw_parts_mut::<u8>(data_ptr as *mut u8, data_size)
                    };
                    match io.direction as u32 {
                        KVM_EXIT_IO_IN => Ok(VcpuExit::IoIn(port, data_slice)),
                        KVM_EXIT_IO_OUT => Ok(VcpuExit::IoOut(port, data_slice)),
                        _ => Err(Error::new(EINVAL)),
                    }
                }
                KVM_EXIT_MMIO => {
                    // Safe because the exit_reason (which comes from the kernel) told us which
                    // union field to use.
                    let mmio = unsafe { run.__bindgen_anon_1.mmio.as_mut() };
                    let addr = mmio.phys_addr;
                    let len = mmio.len as usize;
                    let data_slice = &mut mmio.data[..len];
                    if mmio.is_write != 0 {
                        Ok(VcpuExit::MmioWrite(addr, data_slice))
                    } else {
                        Ok(VcpuExit::MmioRead(addr, data_slice))
                    }
                }
                KVM_EXIT_UNKNOWN => Ok(VcpuExit::Unknown),
                KVM_EXIT_EXCEPTION => Ok(VcpuExit::Exception),
                KVM_EXIT_HYPERCALL => Ok(VcpuExit::Hypercall),
                KVM_EXIT_DEBUG => Ok(VcpuExit::Debug),
                KVM_EXIT_HLT => Ok(VcpuExit::Hlt),
                KVM_EXIT_IRQ_WINDOW_OPEN => Ok(VcpuExit::IrqWindowOpen),
                KVM_EXIT_SHUTDOWN => Ok(VcpuExit::Shutdown),
                KVM_EXIT_FAIL_ENTRY => Ok(VcpuExit::FailEntry),
                KVM_EXIT_INTR => Ok(VcpuExit::Intr),
                KVM_EXIT_SET_TPR => Ok(VcpuExit::SetTpr),
                KVM_EXIT_TPR_ACCESS => Ok(VcpuExit::TprAccess),
                KVM_EXIT_S390_SIEIC => Ok(VcpuExit::S390Sieic),
                KVM_EXIT_S390_RESET => Ok(VcpuExit::S390Reset),
                KVM_EXIT_DCR => Ok(VcpuExit::Dcr),
                KVM_EXIT_NMI => Ok(VcpuExit::Nmi),
                KVM_EXIT_INTERNAL_ERROR => Ok(VcpuExit::InternalError),
                KVM_EXIT_OSI => Ok(VcpuExit::Osi),
                KVM_EXIT_PAPR_HCALL => Ok(VcpuExit::PaprHcall),
                KVM_EXIT_S390_UCONTROL => Ok(VcpuExit::S390Ucontrol),
                KVM_EXIT_WATCHDOG => Ok(VcpuExit::Watchdog),
                KVM_EXIT_S390_TSCH => Ok(VcpuExit::S390Tsch),
                KVM_EXIT_EPR => Ok(VcpuExit::Epr),
                KVM_EXIT_SYSTEM_EVENT => Ok(VcpuExit::SystemEvent),
                r => panic!("unknown kvm exit reason: {}", r),
            }
        } else {
            errno_result()
        }
    }
}

impl AsRawFd for VcpuFd {
    fn as_raw_fd(&self) -> RawFd {
        self.vcpu.as_raw_fd()
    }
}

/// Wrapper for kvm_cpuid2 which has a zero length array at the end.
/// Hides the zero length array behind a bounds check.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[derive(Clone)]
pub struct CpuId {
    bytes: Vec<u8>, // Actually accessed as a kvm_cpuid2 struct.
    allocated_len: usize, // Number of kvm_cpuid_entry2 structs at the end of kvm_cpuid2.
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
impl CpuId {
    pub fn new(array_len: usize) -> CpuId {
        use std::mem::size_of;

        let vec_size_bytes = size_of::<kvm_cpuid2>() + (array_len * size_of::<kvm_cpuid_entry2>());
        let bytes: Vec<u8> = vec![0; vec_size_bytes];
        let kvm_cpuid: &mut kvm_cpuid2 = unsafe {
            // We have ensured in new that there is enough space for the structure so this
            // conversion is safe.
            &mut *(bytes.as_ptr() as *mut kvm_cpuid2)
        };
        kvm_cpuid.nent = array_len as u32;

        CpuId {
            bytes,
            allocated_len: array_len,
        }
    }

    /// Get the entries slice so they can be modified before passing to the VCPU.
    pub fn mut_entries_slice(&mut self) -> &mut [kvm_cpuid_entry2] {
        unsafe {
            // We have ensured in new that there is enough space for the structure so this
            // conversion is safe.
            let kvm_cpuid: &mut kvm_cpuid2 = &mut *(self.bytes.as_ptr() as *mut kvm_cpuid2);

            // Mapping the non-sized array to a slice is unsafe because the length isn't known.
            // Using the length we originally allocated with eliminates the possibility of overflow.
            if kvm_cpuid.nent as usize > self.allocated_len {
                kvm_cpuid.nent = self.allocated_len as u32;
            }
            kvm_cpuid.entries.as_mut_slice(kvm_cpuid.nent as usize)
        }
    }

    /// Get a  pointer so it can be passed to the kernel.  Using this pointer is unsafe.
    pub fn as_ptr(&self) -> *const kvm_cpuid2 {
        self.bytes.as_ptr() as *const kvm_cpuid2
    }

    /// Get a mutable pointer so it can be passed to the kernel.  Using this pointer is unsafe.
    pub fn as_mut_ptr(&mut self) -> *mut kvm_cpuid2 {
        self.bytes.as_mut_ptr() as *mut kvm_cpuid2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    //kvm system related function tests
    #[test]
    fn new() {
        Kvm::new().unwrap();
    }


    #[test]
    fn check_extension() {
        let kvm = Kvm::new().unwrap();
        assert!(kvm.check_extension(Cap::UserMemory));
        // I assume nobody is testing this on s390
        assert!(!kvm.check_extension(Cap::S390UserSigp));
    }

    #[test]
    fn vcpu_mmap_size() {
        let kvm = Kvm::new().unwrap();
        let mmap_size = kvm.get_vcpu_mmap_size().unwrap();
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        assert!(mmap_size >= page_size);
        assert_eq!(mmap_size % page_size, 0);
    }

    #[test]
    fn get_nr_vcpus() {
        let kvm = Kvm::new().unwrap();
        let nr_vcpus = kvm.get_nr_vcpus();
        assert!(nr_vcpus >= 4);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn cpuid_test() {
        let kvm = Kvm::new().unwrap();
        if kvm.check_extension(Cap::ExtCpuid) {
            //what s the maximum cpuid entries? hardcoded value as I see
            let mut cpuid = kvm.get_supported_cpuid(100).unwrap();
            let nr_vcpus = kvm.get_nr_vcpus();
            let mut vm = VmFd::new(&kvm).unwrap();
            for cpu_id in 0..nr_vcpus {
                let vcpu = VcpuFd::new(cpu_id as u8, &mut vm).unwrap();
                vcpu.set_cpuid2(&mut cpuid).unwrap();
            }
        }
    }

    // kvm vm related function tests
    #[test]
    fn create_vm() {
        let kvm = Kvm::new().unwrap();
        VmFd::new(&kvm).unwrap();
    }

    #[test]
    fn get_vm_run_size() {
        let kvm = Kvm::new().unwrap();
        let vm = VmFd::new(&kvm).unwrap();
        assert_eq!(kvm.get_vcpu_mmap_size().unwrap(), vm.get_run_size());
    }

    #[test]
    fn set_invalid_memory_test() {
        let kvm = Kvm::new().unwrap();
        let vm = VmFd::new(&kvm).unwrap();
        assert!(vm.set_user_memory_region(0, 0, 0, 0, 0).is_err());
    }

    #[test]
    fn set_tss_address() {
        let kvm = Kvm::new().unwrap();
        let vm = VmFd::new(&kvm).unwrap();
        assert!(vm.set_tss_address(0xfffbd000).is_ok());

    }

    #[test]
    fn create_irq_chip() {
        let kvm = Kvm::new().unwrap();
        let vm = VmFd::new(&kvm).unwrap();
        assert!(vm.create_irq_chip().is_ok());
    }

    #[test]
    fn create_pit2() {
        let kvm = Kvm::new().unwrap();
        let vm = VmFd::new(&kvm).unwrap();
        assert!(vm.create_pit2().is_ok());
    }

    #[test]
    fn create_vcpu() {
        let kvm = Kvm::new().unwrap();
        let vm = VmFd::new(&kvm).unwrap();
        VcpuFd::new(0, &vm).unwrap();
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn reg_test() {
        let kvm = Kvm::new().unwrap();
        let vm = VmFd::new(&kvm).unwrap();
        let vcpu = VcpuFd::new(0, &vm).unwrap();
        let mut regs = vcpu.get_regs().unwrap();
        regs.rax = 0x1;
        vcpu.set_regs(&regs).unwrap();
        assert!(vcpu.get_regs().unwrap().rax == 0x1);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn sreg_test() {
        let kvm = Kvm::new().unwrap();
        let vm = VmFd::new(&kvm).unwrap();
        let vcpu = VcpuFd::new(0, &vm).unwrap();
        let mut sregs = vcpu.get_sregs().unwrap();
        sregs.cr0 = 0x1;
        sregs.efer = 0x2;

        vcpu.set_sregs(&sregs).unwrap();
        assert_eq!(vcpu.get_sregs().unwrap().cr0, 0x1);
        assert_eq!(vcpu.get_sregs().unwrap().efer, 0x2);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn fpu_test() {
        let kvm = Kvm::new().unwrap();
        let vm = VmFd::new(&kvm).unwrap();
        let vcpu = VcpuFd::new(0, &vm).unwrap();
        let mut fpu: kvm_fpu = kvm_fpu {
            fcw: KVM_FPU_CWD as u16,
            mxcsr: KVM_FPU_MXCSR as u32,
            ..Default::default()
        };

        fpu.fcw = KVM_FPU_CWD as u16;
        fpu.mxcsr = KVM_FPU_MXCSR as u32;

        vcpu.set_fpu(&fpu).unwrap();
        assert_eq!(vcpu.get_fpu().unwrap().fcw, KVM_FPU_CWD as u16);
        //The following will fail; kvm related bug; uncomment when bug solved
        //assert_eq!(vcpu.get_fpu().unwrap().mxcsr, KVM_FPU_MXCSR as u32);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn lapic_test() {
        use std::mem;
        use std::io::Cursor;
        //we might get read of byteorder if we replace 5h3 mem::transmute with something safer
        use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
        //as per https://github.com/torvalds/linux/arch/x86/kvm/lapic.c
        //try to write and read the APIC_ICR (0x300) register which is non-read only and
        //one can simply write to it
        let kvm = Kvm::new().unwrap();
        assert!(kvm.check_extension(Cap::Irqchip));
        let vm = VmFd::new(&kvm).unwrap();
        //the get_lapic ioctl will fail if there is no irqchip created beforehand
        assert!(vm.create_irq_chip().is_ok());
        let vcpu = VcpuFd::new(0, &vm).unwrap();
        let mut klapic: kvm_lapic_state = vcpu.get_lapic().unwrap();

        let reg_offset = 0x300;
        let value = 2 as u32;
        //try to write and read the APIC_ICR	0x300
        let write_slice =
            unsafe { mem::transmute::<&mut [i8], &mut [u8]>(&mut klapic.regs[reg_offset..]) };
        let mut writer = Cursor::new(write_slice);
        writer.write_u32::<LittleEndian>(value).unwrap();
        vcpu.set_lapic(&klapic).unwrap();
        klapic = vcpu.get_lapic().unwrap();
        let read_slice = unsafe { mem::transmute::<&[i8], &[u8]>(&klapic.regs[reg_offset..]) };
        let mut reader = Cursor::new(read_slice);
        assert_eq!(reader.read_u32::<LittleEndian>().unwrap(), value);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn msrs_test() {
        use std::mem;
        let kvm = Kvm::new().unwrap();
        let vm = VmFd::new(&kvm).unwrap();
        let vcpu = VcpuFd::new(0, &vm).unwrap();
        let mut entry_vec = Vec::<kvm_msr_entry>::new();

        entry_vec.push(kvm_msr_entry {
            index: 0x00000174,
            data: 0x0,
            ..Default::default()
        });
        entry_vec.push(kvm_msr_entry {
            index: 0x00000175,
            data: 0x1,
            ..Default::default()
        });

        let vec_size_bytes = mem::size_of::<kvm_msrs>() +
            (entry_vec.len() * mem::size_of::<kvm_msr_entry>());
        let vec: Vec<u8> = Vec::with_capacity(vec_size_bytes);
        let msrs: &mut kvm_msrs = unsafe { &mut *(vec.as_ptr() as *mut kvm_msrs) };
        unsafe {
            let entries: &mut [kvm_msr_entry] = msrs.entries.as_mut_slice(entry_vec.len());
            entries.copy_from_slice(&entry_vec);
        }
        msrs.nmsrs = entry_vec.len() as u32;
        vcpu.set_msrs(msrs).unwrap();

        //now test that GET_MSRS returns the same
        let msrs2: &mut kvm_msrs = &mut vcpu.get_msrs().unwrap();
        let kvm_msr_entries: &mut [kvm_msr_entry] =
            unsafe { msrs2.entries.as_mut_slice(msrs2.nmsrs as usize) };

        for (i, entry) in kvm_msr_entries.iter_mut().enumerate() {
            assert_eq!(entry, &mut entry_vec[i]);
        }
    }
}