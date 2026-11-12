use crate::hypervisor::Vm;
use std::{ffi::c_void, io, ptr};

/// Guest RAM: anonymous host memory mapped into the guest in one or more
/// regions. Unlike memory from `hv_vm_allocate`, macOS takes its pages back
/// when they are discarded.
pub struct GuestMemory {
    vm: Vm,
    host_addr: *mut u8,
    size: usize,
    /// Guest address and length of each region, back to back in host memory.
    regions: Vec<(u64, usize)>,
}

unsafe impl Send for GuestMemory {}
unsafe impl Sync for GuestMemory {}

fn out_of_range() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "guest address outside RAM")
}

impl GuestMemory {
    /// Maps zeroed RAM for `regions` of guest addresses and lengths into `vm`.
    pub fn new(vm: &Vm, regions: Vec<(u64, usize)>) -> Self {
        let size = regions.iter().map(|(_, len)| len).sum();
        let host_addr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_PRIVATE,
                -1,
                0,
            )
        };
        assert_ne!(host_addr, libc::MAP_FAILED, "mmap of guest RAM failed: {}", io::Error::last_os_error());
        let host_addr = host_addr.cast::<u8>();
        let mut offset = 0;
        for &(guest_addr, len) in &regions {
            vm.map(unsafe { host_addr.add(offset) }, guest_addr, len, true)
                .unwrap_or_else(|error| panic!("mapping guest RAM failed: {error}"));
            offset += len;
        }
        Self {
            vm: vm.clone(),
            host_addr,
            size,
            regions,
        }
    }

    /// The VM this RAM is mapped into.
    pub fn vm(&self) -> &Vm {
        &self.vm
    }

    pub fn checked_host_addr(&self, guest_addr: u64, len: usize) -> io::Result<*mut u8> {
        let end = guest_addr.checked_add(len as u64).ok_or_else(out_of_range)?;
        let mut offset = 0;
        for &(start, region_len) in &self.regions {
            if guest_addr >= start && end <= start + region_len as u64 {
                return Ok(unsafe { self.host_addr.add(offset + (guest_addr - start) as usize) });
            }
            offset += region_len;
        }
        Err(out_of_range())
    }

    /// Releases the host pages behind guest RAM the guest no longer uses, so they
    /// stop counting toward the VM's memory. A later guest access reads zeroes or
    /// stale data. Only whole host pages inside the range are released.
    pub fn discard(&self, guest_addr: u64, len: usize) -> io::Result<()> {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        let start = guest_addr.next_multiple_of(page);
        let end = (guest_addr + len as u64) / page * page;
        if end <= start {
            return Ok(());
        }
        let len = (end - start) as usize;
        let host_addr = self.checked_host_addr(start, len)?;
        self.vm.discard(host_addr, start, len)
    }

    pub fn read(&self, guest_addr: u64, data: &mut [u8]) -> io::Result<()> {
        let host_addr = self.checked_host_addr(guest_addr, data.len())?;
        unsafe { ptr::copy_nonoverlapping(host_addr, data.as_mut_ptr(), data.len()) };
        Ok(())
    }

    pub fn write(&self, guest_addr: u64, data: &[u8]) -> io::Result<()> {
        let host_addr = self.checked_host_addr(guest_addr, data.len())?;
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), host_addr, data.len()) };
        Ok(())
    }

    fn read_value<T: Copy>(&self, guest_addr: u64) -> io::Result<T> {
        let host_addr = self.checked_host_addr(guest_addr, size_of::<T>())?;
        Ok(unsafe { ptr::read_unaligned(host_addr.cast::<T>()) })
    }

    pub fn read_u16(&self, guest_addr: u64) -> io::Result<u16> {
        self.read_value(guest_addr)
    }

    pub fn read_u32(&self, guest_addr: u64) -> io::Result<u32> {
        self.read_value(guest_addr)
    }

    pub fn read_u64(&self, guest_addr: u64) -> io::Result<u64> {
        self.read_value(guest_addr)
    }

    pub fn write_u8(&self, guest_addr: u64, value: u8) -> io::Result<()> {
        self.write(guest_addr, &[value])
    }

    pub fn write_u16(&self, guest_addr: u64, value: u16) -> io::Result<()> {
        self.write(guest_addr, &value.to_le_bytes())
    }

    pub fn write_u32(&self, guest_addr: u64, value: u32) -> io::Result<()> {
        self.write(guest_addr, &value.to_le_bytes())
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        for &(guest_addr, len) in &self.regions {
            self.vm
                .unmap(guest_addr, len)
                .unwrap_or_else(|error| panic!("unmapping guest RAM failed: {error}"));
        }
        let result = unsafe { libc::munmap(self.host_addr.cast::<c_void>(), self.size) };
        assert_eq!(result, 0, "munmap of guest RAM failed: {}", io::Error::last_os_error());
    }
}
