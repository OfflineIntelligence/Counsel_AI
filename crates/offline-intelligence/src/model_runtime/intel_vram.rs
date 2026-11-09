//! GPU memory of Intel discrete GPUs (Arc) on Linux.
//!
//! Neither Intel kernel driver exposes VRAM in sysfs on mainline kernels; the
//! dependable source is each driver's memory-region query, an ioctl on the
//! GPU's render node:
//!
//! - `xe` (newer Arc and Battlemage): `DRM_IOCTL_XE_DEVICE_QUERY` with
//!   `DRM_XE_DEVICE_QUERY_MEM_REGIONS`, returning `drm_xe_mem_region`
//!   records; GPU memory is class `DRM_XE_MEM_REGION_CLASS_VRAM`.
//! - `i915` (Arc A-series): `DRM_IOCTL_I915_QUERY` with
//!   `DRM_I915_QUERY_MEMORY_REGIONS`, returning `drm_i915_memory_region_info`
//!   records; GPU memory is class `I915_MEMORY_CLASS_DEVICE`.
//!
//! Layouts and numbers follow the kernel's `include/uapi/drm/xe_drm.h`,
//! `i915_drm.h` and `drm.h`. Free memory needs CAP_PERFMON or CAP_SYS_ADMIN;
//! without it the kernel reports nothing used, so the reading is treated as
//! total memory and the planner keeps its larger reserve. Integrated Intel
//! GPUs have no VRAM region and contribute nothing.
//!
//! Decoding is pure (and tested on every platform); only the ioctl calls
//! are Linux-specific.

/// One GPU-memory region: size and, when the kernel reports it, use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VramRegion {
    pub total: u64,
    /// Bytes in use, when the kernel measured it (None: not permitted).
    pub used: Option<u64>,
}

const DRM_IOCTL_BASE: u64 = b'd' as u64;
const DRM_COMMAND_BASE: u64 = 0x40;
const DRM_XE_DEVICE_QUERY: u64 = 0x00;
const DRM_I915_QUERY: u64 = 0x39;
const DRM_XE_DEVICE_QUERY_MEM_REGIONS: u32 = 1;
const DRM_I915_QUERY_MEMORY_REGIONS: u64 = 4;
const XE_CLASS_VRAM: u16 = 1;
const I915_CLASS_DEVICE: u16 = 1;

/// sizeof(struct drm_xe_device_query): extensions, query, size, data, reserved[2].
const XE_QUERY_SIZE: u64 = 40;
/// sizeof(struct drm_i915_query): num_items, flags, items_ptr.
const I915_QUERY_SIZE: u64 = 16;
/// sizeof(struct drm_xe_mem_region) and of struct drm_i915_memory_region_info.
const REGION_SIZE: usize = 88;
/// Header before the region array: xe (num_mem_regions, pad) and i915 (num_regions, rsvd[3]).
const XE_HEADER: usize = 8;
const I915_HEADER: usize = 16;

/// `_IOWR(type, nr, size)` in the generic Linux encoding (x86, ARM, RISC-V):
/// direction bits 30-31 (read | write), size bits 16-29, type bits 8-15, nr 0-7.
pub const fn iowr(ty: u64, nr: u64, size: u64) -> u64 {
    (3 << 30) | (size << 16) | (ty << 8) | nr
}

pub const XE_DEVICE_QUERY_IOCTL: u64 = iowr(DRM_IOCTL_BASE, DRM_COMMAND_BASE + DRM_XE_DEVICE_QUERY, XE_QUERY_SIZE);
pub const I915_QUERY_IOCTL: u64 = iowr(DRM_IOCTL_BASE, DRM_COMMAND_BASE + DRM_I915_QUERY, I915_QUERY_SIZE);

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// VRAM regions in a `drm_xe_query_mem_regions` blob. Field offsets within a
/// region: mem_class 0, instance 2, min_page_size 4, total_size 8, used 16.
pub fn parse_xe(blob: &[u8]) -> Vec<VramRegion> {
    let n = u32_at(blob, 0).unwrap_or(0) as usize;
    (0..n)
        .filter_map(|i| {
            let o = XE_HEADER + i * REGION_SIZE;
            let class = u16_at(blob, o)?;
            let total = u64_at(blob, o + 8)?;
            let used = u64_at(blob, o + 16)?;
            (class == XE_CLASS_VRAM && total > 0).then_some(VramRegion { total, used: (used > 0).then_some(used) })
        })
        .collect()
}

/// Device-memory regions in a `drm_i915_query_memory_regions` blob. Field
/// offsets within a region: memory_class 0, memory_instance 2, rsvd0 4,
/// probed_size 8, unallocated_size 16.
pub fn parse_i915(blob: &[u8]) -> Vec<VramRegion> {
    let n = u32_at(blob, 0).unwrap_or(0) as usize;
    (0..n)
        .filter_map(|i| {
            let o = I915_HEADER + i * REGION_SIZE;
            let class = u16_at(blob, o)?;
            let probed = u64_at(blob, o + 8)?;
            let unallocated = u64_at(blob, o + 16)?;
            // Without CAP_PERFMON the kernel reports unallocated == probed.
            let used = (unallocated < probed).then(|| probed - unallocated);
            (class == I915_CLASS_DEVICE && probed > 0).then_some(VramRegion { total: probed, used })
        })
        .collect()
}

/// MB of GPU memory over all regions, and whether it is free memory (every
/// region reported its use) or total memory.
pub fn sum_regions(regions: &[VramRegion]) -> Option<(u64, bool)> {
    if regions.is_empty() {
        return None;
    }
    let all_measured = regions.iter().all(|r| r.used.is_some());
    let bytes: u64 = regions
        .iter()
        .map(|r| if all_measured { r.total.saturating_sub(r.used.unwrap_or(0)) } else { r.total })
        .sum();
    Some((bytes / 1024 / 1024, all_measured))
}

/// Every Intel discrete GPU's memory, summed: (MB, GPUs, free rather than total).
#[cfg(target_os = "linux")]
pub fn intel_vram_mb() -> Option<(u64, usize, bool)> {
    let mut regions = Vec::new();
    let mut gpus = 0usize;
    for entry in std::fs::read_dir("/dev/dri").ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("renderD") {
            continue;
        }
        let driver = std::fs::read_link(format!("/sys/class/drm/{}/device/driver", name))
            .ok()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()));
        let found = match driver.as_deref() {
            Some("xe") => linux::query_xe(&entry.path()).map(|b| parse_xe(&b)),
            Some("i915") => linux::query_i915(&entry.path()).map(|b| parse_i915(&b)),
            _ => None,
        };
        if let Some(r) = found.filter(|r| !r.is_empty()) {
            gpus += 1;
            regions.extend(r);
        }
    }
    let (mb, free) = sum_regions(&regions)?;
    Some((mb, gpus, free))
}

#[cfg(target_os = "linux")]
mod linux {
    use std::os::raw::{c_int, c_ulong};
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    #[repr(C)]
    struct XeQuery {
        extensions: u64,
        query: u32,
        size: u32,
        data: u64,
        reserved: [u64; 2],
    }

    #[repr(C)]
    struct I915Query {
        num_items: u32,
        flags: u32,
        items_ptr: u64,
    }

    #[repr(C)]
    struct I915Item {
        query_id: u64,
        length: i32,
        flags: u32,
        data_ptr: u64,
    }

    // The kernel's structs, byte for byte: a mismatch fails the build.
    const _: () = assert!(std::mem::size_of::<XeQuery>() == super::XE_QUERY_SIZE as usize);
    const _: () = assert!(std::mem::size_of::<I915Query>() == super::I915_QUERY_SIZE as usize);
    const _: () = assert!(std::mem::size_of::<I915Item>() == 24);

    /// Two calls: the first (size 0) returns the blob size, the second fills it.
    pub fn query_xe(node: &Path) -> Option<Vec<u8>> {
        let f = std::fs::File::open(node).ok()?;
        let mut q = XeQuery { extensions: 0, query: super::DRM_XE_DEVICE_QUERY_MEM_REGIONS, size: 0, data: 0, reserved: [0; 2] };
        // SAFETY: q is a valid drm_xe_device_query; with size 0 the kernel only writes q.size.
        if unsafe { ioctl(f.as_raw_fd(), super::XE_DEVICE_QUERY_IOCTL as c_ulong, &mut q as *mut XeQuery) } != 0 || q.size == 0 {
            return None;
        }
        let mut buf = vec![0u8; q.size as usize];
        q.data = buf.as_mut_ptr() as u64;
        // SAFETY: buf holds exactly q.size bytes, the size the kernel asked for.
        if unsafe { ioctl(f.as_raw_fd(), super::XE_DEVICE_QUERY_IOCTL as c_ulong, &mut q as *mut XeQuery) } != 0 {
            return None;
        }
        Some(buf)
    }

    pub fn query_i915(node: &Path) -> Option<Vec<u8>> {
        let f = std::fs::File::open(node).ok()?;
        let mut item = I915Item { query_id: super::DRM_I915_QUERY_MEMORY_REGIONS, length: 0, flags: 0, data_ptr: 0 };
        let mut q = I915Query { num_items: 1, flags: 0, items_ptr: &mut item as *mut I915Item as u64 };
        // SAFETY: one valid item with length 0: the kernel only writes item.length.
        if unsafe { ioctl(f.as_raw_fd(), super::I915_QUERY_IOCTL as c_ulong, &mut q as *mut I915Query) } != 0 || item.length <= 0 {
            return None;
        }
        let mut buf = vec![0u8; item.length as usize];
        item.data_ptr = buf.as_mut_ptr() as u64;
        // SAFETY: buf holds exactly item.length bytes, the size the kernel asked for.
        if unsafe { ioctl(f.as_raw_fd(), super::I915_QUERY_IOCTL as c_ulong, &mut q as *mut I915Query) } != 0 || item.length <= 0 {
            return None;
        }
        Some(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn region(class: u16, a: u64, b: u64) -> Vec<u8> {
        let mut r = vec![0u8; REGION_SIZE];
        r[0..2].copy_from_slice(&class.to_ne_bytes());
        r[8..16].copy_from_slice(&a.to_ne_bytes());
        r[16..24].copy_from_slice(&b.to_ne_bytes());
        r
    }

    fn blob(header: usize, regions: &[Vec<u8>]) -> Vec<u8> {
        let mut b = vec![0u8; header];
        b[0..4].copy_from_slice(&(regions.len() as u32).to_ne_bytes());
        for r in regions {
            b.extend_from_slice(r);
        }
        b
    }

    #[test]
    fn ioctl_numbers_match_the_kernel_encoding() {
        // DRM_IOWR(0x40 + 0x39, struct drm_i915_query) with a 16-byte struct.
        assert_eq!(I915_QUERY_IOCTL, 0xC010_6479);
        // DRM_IOWR(0x40 + 0x00, struct drm_xe_device_query) with a 40-byte struct.
        assert_eq!(XE_DEVICE_QUERY_IOCTL, 0xC028_6440);
    }

    #[test]
    fn xe_vram_regions_are_read_and_system_memory_ignored() {
        let b = blob(XE_HEADER, &[region(0, 64 * GIB, 0), region(XE_CLASS_VRAM, 16 * GIB, 4 * GIB)]);
        assert_eq!(parse_xe(&b), vec![VramRegion { total: 16 * GIB, used: Some(4 * GIB) }]);
        assert_eq!(sum_regions(&parse_xe(&b)), Some((12 * 1024, true)));
    }

    #[test]
    fn xe_without_permission_to_measure_use_reports_total_memory() {
        let b = blob(XE_HEADER, &[region(XE_CLASS_VRAM, 12 * GIB, 0)]);
        assert_eq!(sum_regions(&parse_xe(&b)), Some((12 * 1024, false)));
    }

    #[test]
    fn i915_device_regions_are_read_and_unallocated_gives_free_memory() {
        let b = blob(I915_HEADER, &[region(0, 32 * GIB, 32 * GIB), region(I915_CLASS_DEVICE, 16 * GIB, 10 * GIB)]);
        assert_eq!(parse_i915(&b), vec![VramRegion { total: 16 * GIB, used: Some(6 * GIB) }]);
        assert_eq!(sum_regions(&parse_i915(&b)), Some((10 * 1024, true)));
        // Without CAP_PERFMON the kernel reports unallocated == probed: total only.
        let b = blob(I915_HEADER, &[region(I915_CLASS_DEVICE, 8 * GIB, 8 * GIB)]);
        assert_eq!(sum_regions(&parse_i915(&b)), Some((8 * 1024, false)));
    }

    #[test]
    fn integrated_gpus_and_short_blobs_contribute_nothing() {
        assert!(parse_xe(&blob(XE_HEADER, &[region(0, 16 * GIB, 0)])).is_empty());
        assert!(parse_i915(&[1, 0, 0, 0]).is_empty(), "a count with no region data");
        assert_eq!(sum_regions(&[]), None);
    }

    #[test]
    fn several_cards_are_summed() {
        let a = parse_xe(&blob(XE_HEADER, &[region(XE_CLASS_VRAM, 16 * GIB, 2 * GIB)]));
        let b = parse_i915(&blob(I915_HEADER, &[region(I915_CLASS_DEVICE, 8 * GIB, 6 * GIB)]));
        let all: Vec<_> = a.into_iter().chain(b).collect();
        assert_eq!(sum_regions(&all), Some(((14 + 6) * 1024, true)));
    }
}
